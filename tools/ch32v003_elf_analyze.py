#!/usr/bin/env python3
"""
Analyze RISC-V ELF firmware built for WCH CH32V003 (riscv32imc / QingKe V2).

Reports memory usage against the project's memory.x limits, validates that the
binary looks linkable for embedded flash, and disassembles executable sections
with Capstone (RV32I + M + C as used by this target).
"""

from __future__ import annotations

import argparse
import sys
from collections import Counter
from dataclasses import dataclass, field
from pathlib import Path
from typing import BinaryIO, Iterable

from capstone import Cs, CS_ARCH_RISCV, CS_MODE_RISCVC, CS_MODE_RISCV32
from capstone import riscv as riscv_const
from elftools.elf.constants import P_FLAGS, SH_FLAGS
from elftools.elf.elffile import ELFFile

# CH32V003 limits (match memory.x in repo root)
FLASH_ORIGIN = 0x0000_0000
FLASH_SIZE = 16 * 1024
RAM_ORIGIN = 0x2000_0000
RAM_SIZE = 2 * 1024

DEFAULT_ELF = (
    Path(__file__).resolve().parents[1]
    / "target"
    / "riscv32imc-unknown-none-elf"
    / "release"
    / "ch32v003_ili9341_dma"
)

EM_RISCV_NAME = "EM_RISCV"
EM_RISCV_NUM = 243

@dataclass
class MemoryReport:
    text: int = 0
    rodata: int = 0
    data_vma: int = 0
    data_lma: int = 0
    bss: int = 0
    stack: int = 0
    other_flash: int = 0
    other_ram: int = 0

    @property
    def flash_used(self) -> int:
        return self.text + self.rodata + self.data_lma + self.other_flash

    @property
    def ram_used(self) -> int:
        return self.data_vma + self.bss + self.stack + self.other_ram


@dataclass
class ElfSummary:
    path: Path
    elf_class: int
    entry: int
    machine: int
    e_type: int
    memory: MemoryReport = field(default_factory=MemoryReport)
    sections: list[tuple[str, int, int, int]] = field(default_factory=list)
    warnings: list[str] = field(default_factory=list)
    isa_hint: str | None = None


@dataclass
class DisasmReport:
    opcode_counter: Counter
    addr_mode_counter: Counter
    insn_count: int
    sections_disassembled: list[str]


def rv32imc_mnemonic_universe(md: Cs) -> frozenset[str]:
    """All Capstone RV32IMC mnemonics (I + M + C, 32-bit only)."""
    names: set[str] = set()
    for attr in dir(riscv_const):
        if not attr.startswith("RISCV_INS_") or attr == "RISCV_INS_INVALID":
            continue
        insn_id = getattr(riscv_const, attr)
        mnem = md.insn_name(insn_id)
        if mnem and _is_rv32imc_mnemonic(mnem):
            names.add(mnem)
    return frozenset(names)


# Capstone often prints assembler pseudos; include them in the unused-opcode universe.
CAPSTONE_PSEUDOS = frozenset({
    "beqz", "bnez", "bltz", "bgez", "blez", "bgtz",
    "call", "j", "jr", "ret", "tail",
    "csrr", "csrw", "csrwi",
    "li", "lla", "la", "mv", "neg", "nop", "not", "sext.w", "seqz", "snez",
})

RV64_ONLY = frozenset({
    "addw", "subw", "addiw", "slliw", "srliw", "sraiw",
    "ld", "sd", "lwu", "slli.uw",
})


def _is_rv32imc_mnemonic(mnemonic: str) -> bool:
    m = mnemonic.lower()
    if m in CAPSTONE_PSEUDOS:
        return True
    if m.startswith(("amo", "cbo", "v", "lr.", "sc.")):
        return False
    if m.startswith("f") and m not in ("fence", "fence.i"):
        return False
    if m.startswith("c.f") or m in (
        "c.ld", "c.ldsp", "c.sd", "c.sdsp", "c.addiw", "c.addw", "c.subw",
    ):
        return False
    if m in RV64_ONLY or m in ("fence.tso", "sfence.vma"):
        return False
    if ".d" in m:
        return False
    if m.endswith("w") and not m.startswith("c.") and m not in ("lw", "sw"):
        return False
    return True


def classify_insn(mnemonic: str, _operands_str: str) -> str:
    """Classify a RISC-V instruction by addressing / operand style."""
    m = mnemonic.lower()

    if m in (
        "beq", "bne", "blt", "bge", "bltu", "bgeu",
        "jal", "jalr",
        "c.j", "c.jal", "c.jr", "c.jalr", "c.beqz", "c.bnez",
    ):
        return "PC-relative"

    if m.startswith("l") and m not in ("lui", "la", "li", "lla"):
        return "Load with offset"
    if m.startswith("s") and m not in (
        "slli", "srli", "srai", "slt", "sltu", "slti", "sltiu",
    ):
        return "Store with offset"

    if m in ("lui", "auipc"):
        return "Upper-immediate"

    if m in ("addi", "slti", "sltiu", "xori", "ori", "andi", "slli", "srli", "srai", "nop"):
        return "Immediate"
    if m.startswith("c.") and m in (
        "c.li", "c.lui", "c.addi", "c.addi16sp", "c.addi4spn",
        "c.slli", "c.srli", "c.srai", "c.andi",
    ):
        return "Immediate"
    if m == "c.nop":
        return "None"

    if m.startswith(("csrr", "csrw", "csrs", "csrc")):
        return "CSR"

    if m in ("fence", "fence.i", "ecall", "ebreak", "c.ebreak", "wfi"):
        return "System"

    return "Register"


def _in_flash(addr: int, size: int) -> bool:
    if size == 0:
        return True
    end = addr + size
    return addr >= FLASH_ORIGIN and end <= FLASH_ORIGIN + FLASH_SIZE


def _in_ram(addr: int, size: int) -> bool:
    if size == 0:
        return True
    end = addr + size
    return addr >= RAM_ORIGIN and end <= RAM_ORIGIN + RAM_SIZE


def _read_riscv_attributes(elf: ELFFile) -> str | None:
    sec = elf.get_section_by_name(".riscv.attributes")
    if sec is None or sec.header.sh_size == 0:
        return None
    raw = sec.data()
    # Tag file: 'riscv' + subsections; keep a short printable hint
    try:
        text = raw.decode("ascii", errors="replace")
        if "rv32" in text or "arch" in text:
            return text.strip()[:120]
    except Exception:
        pass
    return f"{len(raw)} bytes"


def _section_sizes(elf: ELFFile) -> MemoryReport:
    mem = MemoryReport()
    for section in elf.iter_sections():
        name = section.name
        size = section.header.sh_size
        if size == 0:
            continue
        if name == ".text" or name.startswith(".text."):
            mem.text += size
        elif name == ".rodata" or name.startswith(".rodata."):
            mem.rodata += size
        elif name == ".data":
            mem.data_vma += size
        elif name == ".bss":
            mem.bss += size
        elif name == ".stack":
            mem.stack += size
        elif section.header.sh_flags & SH_FLAGS.SHF_ALLOC:
            addr = section.header.sh_addr
            if _in_ram(addr, size):
                mem.other_ram += size
            elif _in_flash(addr, size):
                mem.other_flash += size
    # Initialized .data is also stored in the ELF file (counts toward flash budget)
    mem.data_lma = mem.data_vma
    return mem


def summarize_elf(elf: ELFFile, path: Path) -> ElfSummary:
    summary = ElfSummary(
        path=path,
        elf_class=elf.elfclass,
        entry=elf.header["e_entry"],
        machine=elf.header["e_machine"],
        e_type=elf.header["e_type"],
    )

    machine = summary.machine
    if machine not in (EM_RISCV_NAME, EM_RISCV_NUM):
        summary.warnings.append(f"e_machine={machine!r} (expected RISC-V)")
    if summary.elf_class != 32:
        summary.warnings.append(f"ELF class {summary.elf_class} (CH32V003 firmware is RV32)")

    summary.isa_hint = _read_riscv_attributes(elf)
    summary.memory = _section_sizes(elf)

    has_exec = False
    has_load_in_flash = False

    for section in elf.iter_sections():
        name = section.name
        addr = section.header.sh_addr
        size = section.header.sh_size
        flags = section.header.sh_flags
        if size == 0 and not (flags & SH_FLAGS.SHF_ALLOC):
            continue
        summary.sections.append((name, addr, size, flags))
        if flags & SH_FLAGS.SHF_EXECINSTR:
            has_exec = True
            if size and not _in_flash(addr, size):
                summary.warnings.append(
                    f"executable section {name!r} at {addr:#x} size {size} outside flash"
                )

    for seg in elf.iter_segments():
        if seg["p_type"] != "PT_LOAD":
            continue
        vaddr = seg["p_vaddr"]
        memsz = seg["p_memsz"]
        if seg["p_flags"] & P_FLAGS.PF_X:
            has_exec = True
        if _in_flash(vaddr, memsz):
            has_load_in_flash = True
        elif memsz and not _in_ram(vaddr, memsz):
            summary.warnings.append(
                f"PT_LOAD at {vaddr:#x} len {memsz} — not in CH32V003 flash or RAM"
            )

    mem = summary.memory
    if mem.text == 0:
        if summary.entry == 0:
            summary.warnings.append("entry point is 0x0 and no .text — firmware not linked")
    elif not _in_flash(summary.entry, 4) and summary.entry != 0:
        summary.warnings.append(f"entry {summary.entry:#x} is outside flash ({FLASH_ORIGIN:#x})")

    if mem.text == 0 and not has_exec:
        summary.warnings.append(
            "no .text / executable content — use REGION_ALIAS in memory.x and link with riscv-rt link.x"
        )
    if not has_load_in_flash and mem.text == 0:
        summary.warnings.append("no PT_LOAD segment in flash range")

    if mem.flash_used > FLASH_SIZE:
        summary.warnings.append(f"flash overflow: {mem.flash_used} > {FLASH_SIZE} bytes")
    if mem.ram_used > RAM_SIZE:
        summary.warnings.append(f"RAM overflow: {mem.ram_used} > {RAM_SIZE} bytes")

    return summary


def make_disassembler(elf_class: int) -> Cs:
    if elf_class != 32:
        raise ValueError(f"CH32V003 requires 32-bit ELF, got ELFCLASS{elf_class}")
    md = Cs(CS_ARCH_RISCV, CS_MODE_RISCV32 | CS_MODE_RISCVC)
    md.detail = True
    return md


def disassemble_elf(elf: ELFFile, md: Cs) -> DisasmReport:
    opcode_counter: Counter = Counter()
    addr_mode_counter: Counter = Counter()
    insn_count = 0
    disassembled: list[str] = []

    for section in elf.iter_sections():
        if not (section.header.sh_flags & SH_FLAGS.SHF_EXECINSTR):
            continue
        data = section.data()
        if not data:
            continue
        base = section.header.sh_addr
        disassembled.append(section.name)
        for insn in md.disasm(data, base):
            mnem = insn.mnemonic
            opcode_counter[mnem] += 1
            addr_mode_counter[classify_insn(mnem, insn.op_str)] += 1
            insn_count += 1

    return DisasmReport(
        opcode_counter=opcode_counter,
        addr_mode_counter=addr_mode_counter,
        insn_count=insn_count,
        sections_disassembled=disassembled,
    )


def _fmt_size(n: int) -> str:
    if n >= 1024:
        return f"{n} ({n / 1024:.2f} KiB)"
    return str(n)


def _pct(part: int, total: int) -> str:
    if total == 0:
        return "  n/a"
    return f"{part / total * 100:5.1f}%"


def print_memory_report(summary: ElfSummary) -> None:
    m = summary.memory
    print("=" * 60)
    print("CH32V003 ELF memory report")
    print("=" * 60)
    print(f"  File:   {summary.path}")
    print(f"  Entry:  {summary.entry:#010x}")
    if summary.isa_hint:
        print(f"  ISA:    {summary.isa_hint}")

    print("\n  Section sizes:")
    print(f"    .text     {_fmt_size(m.text):>12s}")
    print(f"    .rodata   {_fmt_size(m.rodata):>12s}")
    print(f"    .data     {_fmt_size(m.data_vma):>12s}  (VMA in RAM)")
    print(f"    .bss      {_fmt_size(m.bss):>12s}")
    if m.stack:
        print(f"    .stack    {_fmt_size(m.stack):>12s}  (reserved in RAM)")
    flash = m.flash_used
    ram = m.ram_used
    print(f"\n  Flash used: {_fmt_size(flash):>8s} / {FLASH_SIZE} bytes  {_pct(flash, FLASH_SIZE)}")
    print(f"  RAM used:   {_fmt_size(ram):>8s} / {RAM_SIZE} bytes  {_pct(ram, RAM_SIZE)}")

    if flash > FLASH_SIZE:
        print(f"\n  *** FLASH overflow by {flash - FLASH_SIZE} bytes ***")
    if ram > RAM_SIZE:
        print(f"\n  *** RAM overflow by {ram - RAM_SIZE} bytes ***")

    if summary.warnings:
        print("\n  Warnings:")
        for w in summary.warnings:
            print(f"    - {w}")


def _md_escape(cell: str) -> str:
    return cell.replace("|", "\\|")


def _print_markdown_table(headers: list[str], rows: list[list[str]]) -> None:
    widths = [len(h) for h in headers]
    for row in rows:
        for i, cell in enumerate(row):
            widths[i] = max(widths[i], len(cell))
    sep = "|" + "|".join("-" * (w + 2) for w in widths) + "|"
    head = "|" + "|".join(f" {headers[i]:<{widths[i]}} " for i in range(len(headers))) + "|"
    print(head)
    print(sep)
    for row in rows:
        print("|" + "|".join(f" {_md_escape(row[i]):<{widths[i]}} " for i in range(len(row))) + "|")


def print_disasm_report(report: DisasmReport, md: Cs) -> None:
    print("\n## RISC-V disassembly (RV32IMC)\n")
    if not report.sections_disassembled:
        print("(no executable sections disassembled)\n")
        return

    total = report.insn_count
    print(f"- **Sections:** {', '.join(report.sections_disassembled)}")
    print(f"- **Instructions:** {total}")
    print(f"- **Distinct mnemonics used:** {len(report.opcode_counter)}\n")

    used_rows: list[list[str]] = []
    for mnem, cnt in report.opcode_counter.most_common():
        pct = cnt / total * 100 if total else 0.0
        used_rows.append([mnem, str(cnt), f"{pct:.2f}"])

    print("### Opcodes used\n")
    _print_markdown_table(["Mnemonic", "Count", "%"], used_rows)

    universe = rv32imc_mnemonic_universe(md)
    used_set = set(report.opcode_counter.keys())
    unused = sorted(universe - used_set)

    print(f"\n### Opcodes not used (RV32IMC + Capstone pseudos, n={len(unused)})\n")
    if unused:
        _print_markdown_table(["Mnemonic"], [[m] for m in unused])
    else:
        print("(none — every listed mnemonic appears at least once)\n")

    print("\n### Addressing modes\n")
    mode_rows: list[list[str]] = []
    for mode, cnt in report.addr_mode_counter.most_common():
        pct = cnt / total * 100 if total else 0.0
        mode_rows.append([mode, str(cnt), f"{pct:.2f}"])
    _print_markdown_table(["Mode", "Count", "%"], mode_rows)


def analyze(path: Path, disasm: bool) -> int:
    if not path.is_file():
        print(f"error: not a file: {path}", file=sys.stderr)
        return 1

    with path.open("rb") as f:
        return _analyze_file(f, path, disasm)


def _analyze_file(f: BinaryIO, path: Path, disasm: bool) -> int:
    elf = ELFFile(f)
    summary = summarize_elf(elf, path)
    print_memory_report(summary)

    if disasm:
        try:
            cs = make_disassembler(summary.elf_class)
        except ValueError as e:
            print(f"\nerror: {e}", file=sys.stderr)
            return 1
        f.seek(0)
        elf = ELFFile(f)
        report = disassemble_elf(elf, cs)
        print_disasm_report(report, cs)

    return 0


def main(argv: Iterable[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="Analyze CH32V003 (riscv32imc) RISC-V ELF firmware.",
    )
    parser.add_argument(
        "elf",
        nargs="?",
        type=Path,
        default=DEFAULT_ELF,
        help=f"ELF file (default: {DEFAULT_ELF})",
    )
    parser.add_argument(
        "--no-disasm",
        action="store_true",
        help="Skip Capstone disassembly (memory report only)",
    )
    args = parser.parse_args(list(argv) if argv is not None else None)
    return analyze(args.elf, disasm=not args.no_disasm)


if __name__ == "__main__":
    sys.exit(main())
