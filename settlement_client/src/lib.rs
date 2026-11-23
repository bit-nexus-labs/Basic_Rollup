use solana_instruction::{AccountMeta, Instruction};
use solana_pubkey::Pubkey;
use solana_system_interface::program as system_program;

pub fn config_address(program: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"config"], program).0
}

pub fn vault_address(program: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"vault"], program).0
}

pub fn ledger_address(program: &Pubkey, wallet: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"ledger", wallet.as_ref()], program).0
}

fn ix(program: &Pubkey, tag: u8, data_tail: &[u8], accounts: Vec<AccountMeta>) -> Instruction {
    let mut data = Vec::with_capacity(1 + data_tail.len());
    data.push(tag);
    data.extend_from_slice(data_tail);
    Instruction {
        program_id: *program,
        accounts,
        data,
    }
}

#[rustfmt::skip]
pub fn initialize_instruction(program: &Pubkey, payer: &Pubkey, sequencer: &Pubkey) -> Instruction {
    ix(program, 0, sequencer.as_ref(), vec![
        AccountMeta::new(*payer, true),
        AccountMeta::new(config_address(program), false),
        AccountMeta::new(vault_address(program), false),
        AccountMeta::new_readonly(system_program::ID, false),
    ])
}

#[rustfmt::skip]
pub fn deposit_instruction(program: &Pubkey, wallet: &Pubkey, amount: u64) -> Instruction {
    ix(program, 1, &amount.to_le_bytes(), vec![
        AccountMeta::new(*wallet, true),
        AccountMeta::new(config_address(program), false),
        AccountMeta::new(vault_address(program), false),
        AccountMeta::new(ledger_address(program, wallet), false),
        AccountMeta::new_readonly(system_program::ID, false),
    ])
}

#[rustfmt::skip]
pub fn leave_instruction(program: &Pubkey, sequencer: &Pubkey, wallet: &Pubkey, amount: u64) -> Instruction {
    ix(program, 2, &amount.to_le_bytes(), vec![
        AccountMeta::new(*sequencer, true),
        AccountMeta::new(config_address(program), false),
        AccountMeta::new(vault_address(program), false),
        AccountMeta::new(ledger_address(program, wallet), false),
        AccountMeta::new(*wallet, false),
    ])
}

pub fn parse_ledger(data: &[u8]) -> Option<(Pubkey, u64)> {
    if data.len() != 40 {
        return None;
    }
    let pubkey = Pubkey::new_from_array(data[..32].try_into().ok()?);
    let amount = u64::from_le_bytes(data[32..40].try_into().ok()?);
    Some((pubkey, amount))
}

pub fn parse_config(data: &[u8]) -> Option<(Pubkey, u64)> {
    parse_ledger(data)
}

pub fn leave_request_message(wallet: &Pubkey, blockhash: &str) -> Vec<u8> {
    format!("rollup-leave:{wallet}:{blockhash}").into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[rustfmt::skip]
    #[test]
    fn test_settlement_client() {
        let (p, w1, w2, seq) = (Pubkey::new_unique(), Pubkey::new_unique(), Pubkey::new_unique(), Pubkey::new_unique());
        let (c, v, l1, l2) = (config_address(&p), vault_address(&p), ledger_address(&p, &w1), ledger_address(&p, &w2));
        assert!(c != v && c != l1 && v != l1 && l1 != l2);

        let i0 = initialize_instruction(&p, &w1, &seq);
        assert_eq!((i0.data[0], i0.data.len()), (0, 33));
        assert_eq!(i0.accounts, vec![
            AccountMeta::new(w1, true), AccountMeta::new(c, false),
            AccountMeta::new(v, false), AccountMeta::new_readonly(system_program::ID, false),
        ]);

        let i1 = deposit_instruction(&p, &w1, 100);
        assert_eq!((i1.data[0], i1.data.len()), (1, 9));
        assert_eq!(i1.accounts, vec![
            AccountMeta::new(w1, true), AccountMeta::new(c, false),
            AccountMeta::new(v, false), AccountMeta::new(l1, false),
            AccountMeta::new_readonly(system_program::ID, false),
        ]);

        let i2 = leave_instruction(&p, &seq, &w1, 50);
        assert_eq!((i2.data[0], i2.data.len()), (2, 9));
        assert_eq!(i2.accounts, vec![
            AccountMeta::new(seq, true), AccountMeta::new(c, false),
            AccountMeta::new(v, false), AccountMeta::new(l1, false), AccountMeta::new(w1, false),
        ]);

        let mut raw = [0u8; 40];
        raw[..32].copy_from_slice(w1.as_ref());
        raw[32..40].copy_from_slice(&777u64.to_le_bytes());
        assert_eq!(parse_ledger(&raw), Some((w1, 777)));
        assert_eq!(parse_config(&raw), Some((w1, 777)));
        assert_eq!(parse_ledger(&raw[..39]), None);
        assert_eq!(parse_ledger(&[0u8; 41]), None);

        let msg = leave_request_message(&w1, "hash123");
        assert_eq!(msg, format!("rollup-leave:{w1}:hash123").into_bytes());
    }
}
