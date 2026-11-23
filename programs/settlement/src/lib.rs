#![allow(unexpected_cfgs)]
#![allow(deprecated)]

use solana_program::{
    account_info::AccountInfo,
    entrypoint::ProgramResult,
    program::{invoke, invoke_signed},
    program_error::ProgramError,
    pubkey::Pubkey,
    rent::Rent,
    system_instruction, system_program,
    sysvar::Sysvar,
};

#[rustfmt::skip]
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettlementError {
    InvalidInstruction = 0, NotSigner = 1, InvalidPda = 2, InvalidOwner = 3,
    AlreadyInitialized = 4, AlreadyDeposited = 5, NotSequencer = 6, OverCommitted = 7,
    InvalidAmount = 8, InvalidAccountData = 9,
}

impl From<SettlementError> for ProgramError {
    fn from(e: SettlementError) -> Self {
        ProgramError::Custom(e as u32)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettlementInstruction {
    Initialize { sequencer: Pubkey },
    Deposit { amount: u64 },
    Leave { amount: u64 },
}

#[rustfmt::skip]
fn parse_u64(b: &[u8]) -> Result<u64, ProgramError> {
    b.try_into().map(u64::from_le_bytes).map_err(|_| SettlementError::InvalidInstruction.into())
}

#[rustfmt::skip]
fn parse_pubkey(b: &[u8]) -> Result<Pubkey, ProgramError> {
    b.try_into().map(Pubkey::new_from_array).map_err(|_| SettlementError::InvalidInstruction.into())
}

impl SettlementInstruction {
    pub fn unpack(input: &[u8]) -> Result<Self, ProgramError> {
        let (&tag, rest) = input
            .split_first()
            .ok_or(SettlementError::InvalidInstruction)?;
        match tag {
            0 if rest.len() == 32 => Ok(Self::Initialize {
                sequencer: parse_pubkey(rest)?,
            }),
            1 if rest.len() == 8 => Ok(Self::Deposit {
                amount: parse_u64(rest)?,
            }),
            2 if rest.len() == 8 => Ok(Self::Leave {
                amount: parse_u64(rest)?,
            }),
            _ => Err(SettlementError::InvalidInstruction.into()),
        }
    }
}

#[cfg(not(feature = "no-entrypoint"))]
solana_program::entrypoint!(process_instruction);

#[rustfmt::skip]
pub fn process_instruction<'a>(
    pid: &Pubkey, accounts: &'a [AccountInfo<'a>], instruction_data: &[u8],
) -> ProgramResult {
    match SettlementInstruction::unpack(instruction_data)? {
        SettlementInstruction::Initialize { sequencer } => process_init(pid, accounts, sequencer),
        SettlementInstruction::Deposit { amount } => process_deposit(pid, accounts, amount),
        SettlementInstruction::Leave { amount } => process_leave(pid, accounts, amount),
    }
}

#[rustfmt::skip]
fn check_acc(acc: &AccountInfo, pda: &Pubkey, owner: &Pubkey) -> ProgramResult {
    if acc.key != pda { Err(SettlementError::InvalidPda.into()) }
    else if acc.owner != owner { Err(SettlementError::InvalidOwner.into()) }
    else { Ok(()) }
}

#[rustfmt::skip]
fn check_cfg_and_vault(cfg: &AccountInfo, vlt: &AccountInfo, pid: &Pubkey) -> ProgramResult {
    let (cfg_pda, _) = Pubkey::find_program_address(&[b"config"], pid);
    let (vlt_pda, _) = Pubkey::find_program_address(&[b"vault"], pid);
    check_acc(cfg, &cfg_pda, pid)?;
    check_acc(vlt, &vlt_pda, pid)
}

#[rustfmt::skip]
fn check_sys(sys: &AccountInfo) -> ProgramResult {
    if sys.key != &system_program::ID { Err(ProgramError::IncorrectProgramId) } else { Ok(()) }
}

#[rustfmt::skip]
fn write_pda_data(acc: &AccountInfo, key: &Pubkey, val: u64) -> ProgramResult {
    let mut d = acc.try_borrow_mut_data()?;
    if d.len() < 40 { return Err(SettlementError::InvalidAccountData.into()); }
    d[..32].copy_from_slice(key.as_ref());
    d[32..40].copy_from_slice(&val.to_le_bytes());
    Ok(())
}

#[rustfmt::skip]
fn init_pda<'a>(
    payer: &AccountInfo<'a>, target: &AccountInfo<'a>, sys: &AccountInfo<'a>,
    pid: &Pubkey, seeds: &[&[u8]], space: usize,
) -> ProgramResult {
    let req = Rent::get()?.minimum_balance(space);
    let cur = target.lamports();
    let accs = [payer.clone(), target.clone(), sys.clone()];
    if cur == 0 {
        let ix = system_instruction::create_account(payer.key, target.key, req, space as u64, pid);
        return invoke_signed(&ix, &accs, &[seeds]);
    }
    if cur < req {
        invoke(&system_instruction::transfer(payer.key, target.key, req - cur), &accs)?;
    }
    if space > 0 {
        invoke_signed(&system_instruction::allocate(target.key, space as u64), &accs[1..], &[seeds])?;
    }
    invoke_signed(&system_instruction::assign(target.key, pid), &accs[1..], &[seeds])
}

fn process_init<'a>(
    pid: &Pubkey,
    accounts: &'a [AccountInfo<'a>],
    sequencer: Pubkey,
) -> ProgramResult {
    let [payer, cfg, vlt, sys, ..] = accounts else {
        return Err(ProgramError::NotEnoughAccountKeys);
    };
    if !payer.is_signer {
        return Err(SettlementError::NotSigner.into());
    }
    let (cfg_pda, cfg_b) = Pubkey::find_program_address(&[b"config"], pid);
    let (vlt_pda, vlt_b) = Pubkey::find_program_address(&[b"vault"], pid);
    if cfg.key != &cfg_pda || vlt.key != &vlt_pda {
        return Err(SettlementError::InvalidPda.into());
    }
    check_sys(sys)?;
    if !cfg.data_is_empty() || cfg.owner == pid {
        return Err(SettlementError::AlreadyInitialized.into());
    }

    init_pda(payer, cfg, sys, pid, &[b"config", &[cfg_b]], 40)?;
    init_pda(payer, vlt, sys, pid, &[b"vault", &[vlt_b]], 0)?;
    write_pda_data(cfg, &sequencer, 0)
}

fn process_deposit<'a>(
    pid: &Pubkey,
    accounts: &'a [AccountInfo<'a>],
    amount: u64,
) -> ProgramResult {
    if amount == 0 {
        return Err(SettlementError::InvalidAmount.into());
    }
    let [wlt, cfg, vlt, ldg, sys, ..] = accounts else {
        return Err(ProgramError::NotEnoughAccountKeys);
    };
    if !wlt.is_signer {
        return Err(SettlementError::NotSigner.into());
    }
    check_cfg_and_vault(cfg, vlt, pid)?;
    let (ldg_pda, ldg_b) = Pubkey::find_program_address(&[b"ledger", wlt.key.as_ref()], pid);
    if ldg.key != &ldg_pda {
        return Err(SettlementError::InvalidPda.into());
    }
    if ldg.owner == pid && ldg.lamports() > 0 {
        return Err(SettlementError::AlreadyDeposited.into());
    }
    check_sys(sys)?;

    invoke(
        &system_instruction::transfer(wlt.key, vlt.key, amount),
        &[wlt.clone(), vlt.clone(), sys.clone()],
    )?;
    init_pda(
        wlt,
        ldg,
        sys,
        pid,
        &[b"ledger", wlt.key.as_ref(), &[ldg_b]],
        40,
    )?;
    write_pda_data(ldg, wlt.key, amount)?;

    let mut cfg_data = cfg.try_borrow_mut_data()?;
    if cfg_data.len() < 40 {
        return Err(SettlementError::InvalidAccountData.into());
    }
    let cur = parse_u64(&cfg_data[32..40])?;
    let new_tot = cur
        .checked_add(amount)
        .ok_or(ProgramError::ArithmeticOverflow)?;
    cfg_data[32..40].copy_from_slice(&new_tot.to_le_bytes());
    Ok(())
}

fn process_leave<'a>(pid: &Pubkey, accounts: &'a [AccountInfo<'a>], amount: u64) -> ProgramResult {
    let [seq, cfg, vlt, ldg, wlt, ..] = accounts else {
        return Err(ProgramError::NotEnoughAccountKeys);
    };
    if !seq.is_signer {
        return Err(SettlementError::NotSigner.into());
    }
    check_cfg_and_vault(cfg, vlt, pid)?;
    let (ldg_pda, _) = Pubkey::find_program_address(&[b"ledger", wlt.key.as_ref()], pid);
    check_acc(ldg, &ldg_pda, pid)?;

    let ldg_data = ldg.try_borrow_data()?;
    if ldg_data.len() < 40 || &ldg_data[..32] != wlt.key.as_ref() {
        return Err(SettlementError::InvalidAccountData.into());
    }
    drop(ldg_data);

    let mut cfg_data = cfg.try_borrow_mut_data()?;
    if cfg_data.len() < 40 {
        return Err(SettlementError::InvalidAccountData.into());
    }
    if seq.key.as_ref() != &cfg_data[..32] {
        return Err(SettlementError::NotSequencer.into());
    }
    let cur = parse_u64(&cfg_data[32..40])?;
    if amount > cur {
        return Err(SettlementError::OverCommitted.into());
    }
    cfg_data[32..40].copy_from_slice(&(cur - amount).to_le_bytes());
    drop(cfg_data);

    if amount > 0 {
        let v_new = vlt
            .lamports()
            .checked_sub(amount)
            .ok_or(ProgramError::InsufficientFunds)?;
        let w_new = wlt
            .lamports()
            .checked_add(amount)
            .ok_or(ProgramError::ArithmeticOverflow)?;
        **vlt.try_borrow_mut_lamports()? = v_new;
        **wlt.try_borrow_mut_lamports()? = w_new;
    }

    let w_new = wlt
        .lamports()
        .checked_add(ldg.lamports())
        .ok_or(ProgramError::ArithmeticOverflow)?;
    **ldg.try_borrow_mut_lamports()? = 0;
    **wlt.try_borrow_mut_lamports()? = w_new;
    ldg.try_borrow_mut_data()?.fill(0);
    Ok(())
}

#[cfg(test)]
#[rustfmt::skip]
mod tests {
    use super::*;

    #[test]
    fn test_settlement() {
        let seq = Pubkey::new_unique();
        let mut d0 = vec![0u8];
        d0.extend_from_slice(seq.as_ref());
        assert_eq!(SettlementInstruction::unpack(&d0).unwrap(), SettlementInstruction::Initialize { sequencer: seq });
        assert_eq!(SettlementInstruction::unpack(&[1, 1, 0, 0, 0, 0, 0, 0, 0]).unwrap(), SettlementInstruction::Deposit { amount: 1 });
        assert_eq!(SettlementInstruction::unpack(&[2, 1, 0, 0, 0, 0, 0, 0, 0]).unwrap(), SettlementInstruction::Leave { amount: 1 });

        let bads = [&[][..], &[0; 10], &[0; 34], &[1; 8], &[1; 10], &[2; 8], &[2; 10], &[3; 9], &[255]];
        for b in bads { assert!(SettlementInstruction::unpack(b).is_err()); }

        let pid = Pubkey::new_unique();
        let (cfg, _) = Pubkey::find_program_address(&[b"config"], &pid);
        let (vlt, _) = Pubkey::find_program_address(&[b"vault"], &pid);
        let (w1, w2) = (Pubkey::new_unique(), Pubkey::new_unique());
        let (l1, _) = Pubkey::find_program_address(&[b"ledger", w1.as_ref()], &pid);
        let (l2, _) = Pubkey::find_program_address(&[b"ledger", w2.as_ref()], &pid);
        assert!(cfg != vlt && l1 != l2 && l1 != cfg && l1 != vlt);
    }
}
