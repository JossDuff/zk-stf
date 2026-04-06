use sp1_sdk::{blocking::MockProver, blocking::Prover, include_elf, Elf, HashableKey, ProvingKey};

const LEDGER_ELF: Elf = include_elf!("ledger-program");

fn main() {
    let prover = MockProver::new();
    let pk = prover.setup(LEDGER_ELF).expect("failed to setup elf");
    println!("{}", pk.verifying_key().bytes32());
}
