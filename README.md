# zk-stf
Consensus with state transition function validity proven with ZK

## Generate ELF
Turns the program into a RISC-V executable using the succinct Rust toolchain.
```bash
# Generates ELF in target/elf
# This is done automatically by script/src/build.rs
cd program && cargo prove build
```

## Execute program
Without generating a proof.  If execution is successful, then proof generation will probably be successful.
```bash
cd script && RUST_LOG=info cargo run --release -- --execute
```

## Generate proof
```bash
cd script && RUST_LOG=info cargo run --release -- --prove
```
