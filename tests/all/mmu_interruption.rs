#![cfg(not(miri))]

use object::{Object, ObjectSection};
use wasmtime::{Config, Engine, Result, Strategy};
use wasmtime_environ::obj::ELF_WASMTIME_TRAPS;
use wasmtime_environ::{CompiledTrap, iterate_traps};
use wasmtime_test_macros::wasmtime_test;

// Returns the offset of every MMU-interrupt check recorded in a compiled
// module's trap table.
fn mmu_interrupt_checks(elf_bytes: &[u8]) -> Vec<u32> {
    let elf = object::read::elf::ElfFile64::<object::Endianness>::parse(elf_bytes)
        .expect("ELF should be parseable");
    let section = elf
        .section_by_name(ELF_WASMTIME_TRAPS)
        .expect(&format!("{ELF_WASMTIME_TRAPS} section should be present"));

    iterate_traps(section.data().unwrap())
        .expect(&format!("{ELF_WASMTIME_TRAPS} section should be parseable"))
        .filter_map(|(offset, trap)| match trap {
            CompiledTrap::MmuInterrupt => Some(offset),
            _ => None,
        })
        .collect()
}

// A function with an infinite loop contains two MMU-interrupt checks, one in
// the function prologue and another at the loop backedge. If you change this
// wat, change it in mmu-interruption-compile-loop{,-aarch64}.wat, too.
const LOOPING_MODULE: &str = r#"(module
             (memory 0)
             (func (loop (br 0)))
           )"#;

// Asserts that each MMU-interrupt check is recorded in the trap table at the
// offset of its dead load.
#[wasmtime_test(strategies(only(CraneliftNative)))]
fn mmu_interrupt_check_offsets(config: &mut Config) -> Result<()> {
    config.mmu_interruption(true);
    config.target("x86_64-unknown-linux-gnu").unwrap();
    let engine = Engine::new(config).unwrap();

    let elf_bytes = engine.precompile_module(LOOPING_MODULE.as_bytes()).unwrap();
    let starts = mmu_interrupt_checks(&elf_bytes);

    // The emitted machine code is nailed down by the
    // mmu-interruption-compile-loop.wat disas test. As long as that keeps
    // passing, these values remain valid.
    assert_eq!(
        starts,
        vec![12, 15],
        "There should be 2 MMU-interrupt checks (function prologue & loop backedge). The offset of the prologue's dead load should be 12, and that of the loop's backedge should be 15."
    );
    Ok(())
}

// The aarch64 counterpart of `mmu_interrupt_check_offsets`.
#[wasmtime_test(strategies(only(CraneliftNative)))]
fn mmu_interrupt_check_offsets_aarch64(config: &mut Config) -> Result<()> {
    config.mmu_interruption(true);
    config.target("aarch64-unknown-linux-gnu").unwrap();
    let engine = Engine::new(config).unwrap();

    let elf_bytes = engine.precompile_module(LOOPING_MODULE.as_bytes()).unwrap();
    let starts = mmu_interrupt_checks(&elf_bytes);

    // The emitted machine code is nailed down by the
    // mmu-interruption-compile-loop-aarch64.wat disas test. As long as that
    // keeps passing, these values remain valid.
    assert_eq!(
        starts,
        vec![20, 24],
        "There should be 2 MMU-interrupt checks (function prologue & loop backedge). The offset of the prologue's dead load should be 20, and that of the loop's backedge should be 24."
    );
    Ok(())
}

#[test]
fn requires_signals_based_traps() {
    let mut config = Config::new();
    config.mmu_interruption(true);
    config.signals_based_traps(false);
    let err = Engine::new(&config).expect_err("engine creation should fail");
    assert_eq!(
        err.to_string(),
        "MMU interruption requires signals-based traps",
    );
}

// With Cranelift only the x64 and aarch64 backends are supported.
// With Cranelift, only the x64 and aarch64 backends are supported.
#[test]
fn requires_supported_isa() {
    let mut config = Config::new();
    config.mmu_interruption(true);
    config.target("riscv64").unwrap();
    config.signals_based_traps(true);
    let err = Engine::new(&config).expect_err("engine creation should fail");
    assert_eq!(
        err.to_string(),
        "MMU interruption is supported only on x86_64 and aarch64 Linux, not `riscv64-unknown-unknown-elf`",
    );
}

#[test]
fn requires_linux_target() {
    let mut config = Config::new();
    config.mmu_interruption(true);
    config.signals_based_traps(true);
    config.target("x86_64-apple-darwin").unwrap();
    let err = Engine::new(&config).expect_err("engine creation should fail");
    assert_eq!(
        err.to_string(),
        "MMU interruption is supported only on x86_64 and aarch64 Linux, not `x86_64-apple-darwin`",
    );
}

// The Winch backend does not support this feature.
#[test]
fn rejected_by_winch() {
    let mut config = Config::new();
    config.mmu_interruption(true);
    config.strategy(Strategy::Winch);
    config.target("x86_64-unknown-linux-gnu").unwrap();
    let err = Engine::new(&config).expect_err("engine creation should fail");
    assert_eq!(
        err.to_string(),
        "Winch does not currently support MMU interruption",
    );
}

// Pulley does not support this feature, since compiled-in interruption checks
// will be absent.
#[test]
fn rejected_by_pulley() {
    let mut config = Config::new();
    config.mmu_interruption(true);
    config.signals_based_traps(true);
    config.target("pulley64").unwrap();
    let err = Engine::new(&config).expect_err("engine creation should fail");
    assert_eq!(
        err.to_string(),
        "MMU interruption is supported only on x86_64 and aarch64 Linux, not `pulley64-unknown-unknown-elf`",
    );
}

// AOT compilation succeeds for a supported target, regardless of host.
#[test]
fn precompile_succeeds_for_valid_config_on_any_host() {
    let mut config = Config::new();
    config.mmu_interruption(true);
    config.target("x86_64-unknown-linux-gnu").unwrap();
    config.signals_based_traps(true);
    let engine = Engine::new(&config).unwrap();
    engine
        .precompile_module(r#"(module (memory 0) (func))"#.as_bytes())
        .expect("precompilation should succeed regardless of host");
}
