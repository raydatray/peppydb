/*
 * Linker script for every Linux link through `zig cc` (plan 0035).
 *
 * Zig compiles its runtime support from source: the glibc start files and
 * stubs, libunwind (what `-lgcc_s` resolves to), and compiler-rt. It compiles
 * them with DWARF debug information, and clang records the working directory
 * of the compile as DW_AT_comp_dir. That directory is the checkout, so without
 * this script a link on one machine would differ from the same link on
 * another machine with a different checkout path.
 *
 * Those runtime objects are the only link inputs under the Zig cache
 * directories, which build/toolchains/zig_cc.sh names `zig-global-cache` and
 * `zig-local-cache`. This script discards the debug sections of those inputs
 * only. The debug information of all other objects and the symbol table stay.
 *
 * `INSERT` keeps lld's default layout. Without it, a SECTIONS command would
 * replace the built-in linker script.
 *
 * Follows `toolchains/zig/runtime-debug-discard.ld` in rue-language/rue.
 */
SECTIONS {
  /DISCARD/ : {
    *zig-global-cache/*(.debug_*)
    *zig-local-cache/*(.debug_*)
  }
}
INSERT AFTER .text;
