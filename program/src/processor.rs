//! Program state processor

use std::convert::TryInto;

use num_traits::FromPrimitive;
use solana_program::{
    account_info::{AccountInfo, next_account_info},
    clock::{Clock, UnixTimestamp},
    decode_error::DecodeError,
    entrypoint::ProgramResult,
    msg,
    program::invoke_signed,
    program_error::{PrintProgramError, ProgramError},
    program_option::COption,
    program_pack::Pack,
    pubkey::Pubkey,
    sysvar::Sysvar,
};

use crate::{
    curve::{
        base::SwapCurve,
        calculator::{RoundDirection, TradeDirection},
        fees::Fees,
    },
    error::SwapError,
    instruction::{
        DepositAllTokenTypes, DepositSingleTokenTypeExactAmountIn, Initialize,
        InitializeFarming, StartFarming, Swap,
        SwapInstruction, WithdrawAllTokenTypes,
        WithdrawSingleTokenTypeExactAmountOut,
    },
    state::{SwapState, SwapV1, SwapVersion},
};
use crate::constraints::{SWAP_CONSTRAINTS, SwapConstraints};
use crate::error::FarmingError;
use crate::yield_farming::farming_state::{FARMING_STATE_DISCRIMINATOR, FarmingState};
use crate::yield_farming::farming_ticket::{FarmingTicket, TICKET_DISCRIMINATOR};
use crate::yield_farming::snapshots::{QUEUE_LENGTH, Snapshot};

/// Program state handler.
pub struct Processor {}

impl Processor {
    /// Unpacks a spl_token `Account`.
    pub fn unpack_token_account(
        account_info: &AccountInfo,
        token_program_id: &Pubkey,
    ) -> Result<spl_token::state::Account, SwapError> {
        if account_info.owner != token_program_id {
            Err(SwapError::IncorrectTokenProgramId)
        } else {
            spl_token::state::Account::unpack(&account_info.data.borrow())
                .map_err(|_| SwapError::ExpectedAccount)
        }
    }

    /// Unpacks a spl_token `Mint`.
    pub fn unpack_mint(
        account_info: &AccountInfo,
        token_program_id: &Pubkey,
    ) -> Result<spl_token::state::Mint, SwapError> {
        if account_info.owner != token_program_id {
            Err(SwapError::IncorrectTokenProgramId)
        } else {
            spl_token::state::Mint::unpack(&account_info.data.borrow())
                .map_err(|_| SwapError::ExpectedMint)
        }
    }

    /// Calculates the authority id by generating a program address.
    pub fn authority_id(
        program_id: &Pubkey,
        my_info: &Pubkey,
        nonce: u8,
    ) -> Result<Pubkey, SwapError> {
        Pubkey::create_program_address(&[&my_info.to_bytes()[..32], &[nonce]], program_id)
            .or(Err(SwapError::InvalidProgramAddress))
    }

    /// Issue a spl_token `Burn` instruction.
    pub fn token_burn<'a>(
        swap: &Pubkey,
        token_program: AccountInfo<'a>,
        burn_account: AccountInfo<'a>,
        mint: AccountInfo<'a>,
        authority: AccountInfo<'a>,
        nonce: u8,
        amount: u64,
    ) -> Result<(), ProgramError> {
        let swap_bytes = swap.to_bytes();
        let authority_signature_seeds = [&swap_bytes[..32], &[nonce]];
        let signers = &[&authority_signature_seeds[..]];

        let ix = spl_token::instruction::burn(
            token_program.key,
            burn_account.key,
            mint.key,
            authority.key,
            &[],
            amount,
        )?;

        invoke_signed(
            &ix,
            &[burn_account, mint, authority, token_program],
            signers,
        )
    }

    /// Issue a spl_token `MintTo` instruction.
    pub fn token_mint_to<'a>(
        swap: &Pubkey,
        token_program: AccountInfo<'a>,
        mint: AccountInfo<'a>,
        destination: AccountInfo<'a>,
        authority: AccountInfo<'a>,
        nonce: u8,
        amount: u64,
    ) -> Result<(), ProgramError> {
        let swap_bytes = swap.to_bytes();
        let authority_signature_seeds = [&swap_bytes[..32], &[nonce]];
        let signers = &[&authority_signature_seeds[..]];
        let ix = spl_token::instruction::mint_to(
            token_program.key,
            mint.key,
            destination.key,
            authority.key,
            &[],
            amount,
        )?;

        invoke_signed(&ix, &[mint, destination, authority, token_program], signers)
    }

    /// Issue a spl_token `Transfer` instruction.
    pub fn token_transfer<'a>(
        swap: &Pubkey,
        token_program: AccountInfo<'a>,
        source: AccountInfo<'a>,
        destination: AccountInfo<'a>,
        authority: AccountInfo<'a>,
        nonce: u8,
        amount: u64,
    ) -> Result<(), ProgramError> {
        let swap_bytes = swap.to_bytes();
        let authority_signature_seeds = [&swap_bytes[..32], &[nonce]];
        let signers = &[&authority_signature_seeds[..]];
        let ix = spl_token::instruction::transfer(
            token_program.key,
            source.key,
            destination.key,
            authority.key,
            &[],
            amount,
        )?;
        invoke_signed(
            &ix,
            &[source, destination, authority, token_program],
            signers,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn check_accounts(
        token_swap: &dyn SwapState,
        program_id: &Pubkey,
        swap_account_info: &AccountInfo,
        authority_info: &AccountInfo,
        token_a_info: &AccountInfo,
        token_b_info: &AccountInfo,
        pool_mint_info: &AccountInfo,
        token_program_info: &AccountInfo,
        user_token_a_info: Option<&AccountInfo>,
        user_token_b_info: Option<&AccountInfo>,
        pool_fee_account_info: Option<&AccountInfo>,
    ) -> ProgramResult {
        if swap_account_info.owner != program_id {
            return Err(ProgramError::IncorrectProgramId);
        }
        if *authority_info.key
            != Self::authority_id(program_id, swap_account_info.key, token_swap.nonce())?
        {
            return Err(SwapError::InvalidProgramAddress.into());
        }
        if *token_a_info.key != *token_swap.token_a_account() {
            return Err(SwapError::IncorrectSwapAccount.into());
        }
        if *token_b_info.key != *token_swap.token_b_account() {
            return Err(SwapError::IncorrectSwapAccount.into());
        }
        if *pool_mint_info.key != *token_swap.pool_mint() {
            return Err(SwapError::IncorrectPoolMint.into());
        }
        if *token_program_info.key != *token_swap.token_program_id() {
            return Err(SwapError::IncorrectTokenProgramId.into());
        }
        if let Some(user_token_a_info) = user_token_a_info {
            if token_a_info.key == user_token_a_info.key {
                return Err(SwapError::InvalidInput.into());
            }
        }
        if let Some(user_token_b_info) = user_token_b_info {
            if token_b_info.key == user_token_b_info.key {
                return Err(SwapError::InvalidInput.into());
            }
        }
        if let Some(pool_fee_account_info) = pool_fee_account_info {
            if *pool_fee_account_info.key != *token_swap.pool_fee_account() {
                return Err(SwapError::IncorrectFeeAccount.into());
            }
        }
        Ok(())
    }

    fn check_farming_accounts(
        program_id: &Pubkey,
        token_swap: &dyn SwapState,
        swap_account_info: &AccountInfo,
        farming_state: &FarmingState,
        farming_state_info: &AccountInfo,
        token_program_info: Option<&AccountInfo>,
        farming_token_freeze_account: Option<&AccountInfo>,
        farming_token_account: Option<&AccountInfo>,
        farming_ticket: Option<&FarmingTicket>,
        farming_ticket_info: Option<&AccountInfo>,
        user_authority_info: Option<&AccountInfo>,
        authority_info: Option<&AccountInfo>,
    ) -> ProgramResult {
        if swap_account_info.owner != program_id || farming_state_info.owner != program_id {
            return Err(ProgramError::IncorrectProgramId);
        }
        if *token_swap.farming_state() != *farming_state_info.key {
            return Err(ProgramError::InvalidAccountData);
        }
        if let Some(farming_token_freeze_account) = farming_token_freeze_account {
            if *farming_token_freeze_account.key != *token_swap.token_farming_freeze_account() {
                return Err(ProgramError::InvalidAccountData);
            }
        }
        if let Some(farming_token_account) = farming_token_account {
            if *farming_token_account.key != farming_state.farming_token_account {
                return Err(ProgramError::InvalidAccountData);
            }
        }

        if let Some(token_program_info) = token_program_info {
            if *token_program_info.key != *token_swap.token_program_id() {
                return Err(SwapError::IncorrectTokenProgramId.into());
            }
        }

        if let Some(farming_ticket_info) = farming_ticket_info {
            if let Some(farming_ticket) = farming_ticket {
                if farming_ticket.farming_state != *farming_state_info.key {
                    return Err(ProgramError::InvalidAccountData);
                }
                if farming_ticket_info.owner != program_id {
                    return Err(ProgramError::IncorrectProgramId);
                }
            } else {
                return Err(ProgramError::NotEnoughAccountKeys);
            }
        }

        if let Some(farming_ticket) = farming_ticket {
            if let Some(user_authority_info) = user_authority_info {
                if farming_ticket.token_authority != *user_authority_info.key {
                    return Err(ProgramError::MissingRequiredSignature);
                }
            } else {
                return Err(ProgramError::NotEnoughAccountKeys);
            }
        }

        if let Some(authority_info) = authority_info {
            if *authority_info.key
                != Self::authority_id(program_id, swap_account_info.key, token_swap.nonce())?
            {
                return Err(SwapError::InvalidProgramAddress.into());
            }
        }

        Ok(())
    }

    /// Processes an [Initialize](enum.Instruction.html).
    pub fn process_initialize(
        program_id: &Pubkey,
        nonce: u8,
        fees: Fees,
        swap_curve: SwapCurve,
        accounts: &[AccountInfo],
        swap_constraints: &Option<SwapConstraints>,
    ) -> ProgramResult {
        let account_info_iter = &mut accounts.iter();
        let swap_info = next_account_info(account_info_iter)?;
        let authority_info = next_account_info(account_info_iter)?;
        let token_a_info = next_account_info(account_info_iter)?;
        let token_b_info = next_account_info(account_info_iter)?;
        let pool_mint_info = next_account_info(account_info_iter)?;
        let fee_account_info = next_account_info(account_info_iter)?;
        let destination_info = next_account_info(account_info_iter)?;
        let token_program_info = next_account_info(account_info_iter)?;
        let farming_info = next_account_info(account_info_iter)?;
        let token_freeze_info = next_account_info(account_info_iter)?;

        let token_program_id = *token_program_info.key;
        if SwapVersion::is_initialized(&swap_info.data.borrow()) {
            return Err(SwapError::AlreadyInUse.into());
        }

        if *authority_info.key != Self::authority_id(program_id, swap_info.key, nonce)? {
            return Err(SwapError::InvalidProgramAddress.into());
        }

        let token_a = Self::unpack_token_account(token_a_info, &token_program_id)?;
        let token_b = Self::unpack_token_account(token_b_info, &token_program_id)?;

        let token_freeze = Self::unpack_token_account(token_freeze_info, &token_program_id)?;

        let fee_account = Self::unpack_token_account(fee_account_info, &token_program_id)?;
        let destination = Self::unpack_token_account(destination_info, &token_program_id)?;
        let pool_mint = Self::unpack_mint(pool_mint_info, &token_program_id)?;

        if *authority_info.key != token_a.owner {
            return Err(SwapError::InvalidOwner.into());
        }
        if *authority_info.key != token_b.owner {
            return Err(SwapError::InvalidOwner.into());
        }
        if *authority_info.key != token_freeze.owner {
            return Err(SwapError::InvalidOwner.into());
        }
        if *authority_info.key == destination.owner {
            return Err(SwapError::InvalidOutputOwner.into());
        }
        if *authority_info.key == fee_account.owner {
            return Err(SwapError::InvalidOutputOwner.into());
        }
        if COption::Some(*authority_info.key) != pool_mint.mint_authority {
            return Err(SwapError::InvalidOwner.into());
        }

        if token_a.mint == token_b.mint {
            return Err(SwapError::RepeatedMint.into());
        }
        swap_curve
            .calculator
            .validate_supply(token_a.amount, token_b.amount)?;
        if token_freeze.mint != *pool_mint_info.key {
            return Err(SwapError::IncorrectPoolMint.into());
        }
        if token_a.delegate.is_some() {
            return Err(SwapError::InvalidDelegate.into());
        }
        if token_b.delegate.is_some() {
            return Err(SwapError::InvalidDelegate.into());
        }
        if token_freeze.delegate.is_some() {
            return Err(SwapError::InvalidDelegate.into());
        }
        if token_a.close_authority.is_some() {
            return Err(SwapError::InvalidCloseAuthority.into());
        }
        if token_b.close_authority.is_some() {
            return Err(SwapError::InvalidCloseAuthority.into());
        }
        if token_freeze.close_authority.is_some() {
            return Err(SwapError::InvalidCloseAuthority.into());
        }

        if pool_mint.supply != 0 {
            return Err(SwapError::InvalidSupply.into());
        }
        if pool_mint.freeze_authority.is_some() {
            return Err(SwapError::InvalidFreezeAuthority.into());
        }
        if *pool_mint_info.key != fee_account.mint {
            return Err(SwapError::IncorrectPoolMint.into());
        }

        if let Some(swap_constraints) = swap_constraints {
            let owner_key = swap_constraints
                .owner_key
                .parse::<Pubkey>()
                .map_err(|_| SwapError::InvalidOwner)?;
            if fee_account.owner != owner_key {
                return Err(SwapError::InvalidOwner.into());
            }
            swap_constraints.validate_curve(&swap_curve)?;
            swap_constraints.validate_fees(&fees)?;
        }
        fees.validate()?;
        swap_curve.calculator.validate()?;
        let initial_amount = swap_curve.calculator.new_pool_supply();

        Self::token_mint_to(
            swap_info.key,
            token_program_info.clone(),
            pool_mint_info.clone(),
            destination_info.clone(),
            authority_info.clone(),
            nonce,
            to_u64(initial_amount)?,
        )?;

        let farming_state = FarmingState {
            discriminator: u64::from_le_bytes(FARMING_STATE_DISCRIMINATOR),
            is_initialized: false,
            tokens_unlocked: 0,
            tokens_total: 0,
            tokens_per_period: 0,
            period_length: 0,
            start_time: 0,
            current_time: 0,
            attached_swap_account: swap_info.key.clone(),
            farming_token_account: solana_program::system_program::ID,
            farming_snapshots: Default::default(),
        };

        FarmingState::pack(farming_state, &mut farming_info.data.borrow_mut())?;

        let obj = SwapVersion::SwapV1(SwapV1 {
            is_initialized: true,
            nonce,
            token_program_id,
            token_a: *token_a_info.key,
            token_b: *token_b_info.key,
            pool_mint: *pool_mint_info.key,
            token_farming_freeze_account: *token_freeze_info.key,
            token_a_mint: token_a.mint,
            token_b_mint: token_b.mint,
            pool_fee_account: *fee_account_info.key,
            fees,
            swap_curve,
            farming_state: *farming_info.key,
        });
        SwapVersion::pack(obj, &mut swap_info.data.borrow_mut())?;

        Ok(())
    }

    /// Processes an [Swap](enum.Instruction.html).
    pub fn process_swap(
        program_id: &Pubkey,
        amount_in: u64,
        minimum_amount_out: u64,
        accounts: &[AccountInfo],
    ) -> ProgramResult {
        let account_info_iter = &mut accounts.iter();
        let swap_info = next_account_info(account_info_iter)?;
        let authority_info = next_account_info(account_info_iter)?;
        let user_transfer_authority_info = next_account_info(account_info_iter)?;
        let source_info = next_account_info(account_info_iter)?;
        let swap_source_info = next_account_info(account_info_iter)?;
        let swap_destination_info = next_account_info(account_info_iter)?;
        let destination_info = next_account_info(account_info_iter)?;
        let pool_mint_info = next_account_info(account_info_iter)?;
        let pool_fee_account_info = next_account_info(account_info_iter)?;
        let token_program_info = next_account_info(account_info_iter)?;

        if swap_info.owner != program_id {
            return Err(ProgramError::IncorrectProgramId);
        }
        let token_swap = SwapVersion::unpack(&swap_info.data.borrow())?;

        if *authority_info.key != Self::authority_id(program_id, swap_info.key, token_swap.nonce())?
        {
            return Err(SwapError::InvalidProgramAddress.into());
        }
        if !(*swap_source_info.key == *token_swap.token_a_account()
            || *swap_source_info.key == *token_swap.token_b_account())
        {
            return Err(SwapError::IncorrectSwapAccount.into());
        }
        if !(*swap_destination_info.key == *token_swap.token_a_account()
            || *swap_destination_info.key == *token_swap.token_b_account())
        {
            return Err(SwapError::IncorrectSwapAccount.into());
        }
        if *swap_source_info.key == *swap_destination_info.key {
            return Err(SwapError::InvalidInput.into());
        }
        if swap_source_info.key == source_info.key {
            return Err(SwapError::InvalidInput.into());
        }
        if swap_destination_info.key == destination_info.key {
            return Err(SwapError::InvalidInput.into());
        }
        if *pool_mint_info.key != *token_swap.pool_mint() {
            return Err(SwapError::IncorrectPoolMint.into());
        }
        if *pool_fee_account_info.key != *token_swap.pool_fee_account() {
            return Err(SwapError::IncorrectFeeAccount.into());
        }
        if *token_program_info.key != *token_swap.token_program_id() {
            return Err(SwapError::IncorrectTokenProgramId.into());
        }

        let source_account =
            Self::unpack_token_account(swap_source_info, &token_swap.token_program_id())?;
        let dest_account =
            Self::unpack_token_account(swap_destination_info, &token_swap.token_program_id())?;
        let pool_mint = Self::unpack_mint(pool_mint_info, &token_swap.token_program_id())?;

        let trade_direction = if *swap_source_info.key == *token_swap.token_a_account() {
            TradeDirection::AtoB
        } else {
            TradeDirection::BtoA
        };

        let result = token_swap
            .swap_curve()
            .swap(
                to_u128(amount_in)?,
                to_u128(source_account.amount)?,
                to_u128(dest_account.amount)?,
                trade_direction,
                token_swap.fees(),
            )
            .ok_or(SwapError::ZeroTradingTokens)?;
        if result.destination_amount_swapped < to_u128(minimum_amount_out)? {
            return Err(SwapError::ExceededSlippage.into());
        }
        let (swap_token_a_amount, swap_token_b_amount) = match trade_direction {
            TradeDirection::AtoB => (
                result.new_swap_source_amount,
                result.new_swap_destination_amount,
            ),
            TradeDirection::BtoA => (
                result.new_swap_destination_amount,
                result.new_swap_source_amount,
            ),
        };

        Self::token_transfer(
            swap_info.key,
            token_program_info.clone(),
            source_info.clone(),
            swap_source_info.clone(),
            user_transfer_authority_info.clone(),
            token_swap.nonce(),
            to_u64(result.source_amount_swapped)?,
        )?;

        let pool_token_amount = token_swap
            .swap_curve()
            .trading_tokens_to_pool_tokens(
                result.owner_fee,
                swap_token_a_amount,
                swap_token_b_amount,
                to_u128(pool_mint.supply)?,
                trade_direction,
                RoundDirection::Ceiling,
                token_swap.fees(),
            )
            .ok_or(SwapError::FeeCalculationFailure)?;

        if pool_token_amount > 0 {
            Self::token_mint_to(
                swap_info.key,
                token_program_info.clone(),
                pool_mint_info.clone(),
                pool_fee_account_info.clone(),
                authority_info.clone(),
                token_swap.nonce(),
                to_u64(pool_token_amount)?,
            )?;
            // We are minting tokens only for the fees, so that LP token will increase in shares
        }

        Self::token_transfer(
            swap_info.key,
            token_program_info.clone(),
            swap_destination_info.clone(),
            destination_info.clone(),
            authority_info.clone(),
            token_swap.nonce(),
            to_u64(result.destination_amount_swapped)?,
        )?;

        Ok(())
    }

    /// Processes an [DepositAllTokenTypes](enum.Instruction.html).
    pub fn process_deposit_all_token_types(
        program_id: &Pubkey,
        pool_token_amount: u64,
        maximum_token_a_amount: u64,
        maximum_token_b_amount: u64,
        accounts: &[AccountInfo],
    ) -> ProgramResult {
        let account_info_iter = &mut accounts.iter();
        let swap_info = next_account_info(account_info_iter)?;
        let authority_info = next_account_info(account_info_iter)?;
        let user_transfer_authority_info = next_account_info(account_info_iter)?;
        let source_a_info = next_account_info(account_info_iter)?;
        let source_b_info = next_account_info(account_info_iter)?;
        let token_a_info = next_account_info(account_info_iter)?;
        let token_b_info = next_account_info(account_info_iter)?;
        let pool_mint_info = next_account_info(account_info_iter)?;
        let dest_info = next_account_info(account_info_iter)?;
        let token_program_info = next_account_info(account_info_iter)?;

        let token_swap = SwapVersion::unpack(&swap_info.data.borrow())?;
        let calculator = &token_swap.swap_curve().calculator;
        if !calculator.allows_deposits() {
            return Err(SwapError::UnsupportedCurveOperation.into());
        }
        Self::check_accounts(
            token_swap.as_ref(),
            program_id,
            swap_info,
            authority_info,
            token_a_info,
            token_b_info,
            pool_mint_info,
            token_program_info,
            Some(source_a_info),
            Some(source_b_info),
            None,
        )?;

        let token_a = Self::unpack_token_account(token_a_info, &token_swap.token_program_id())?;
        let token_b = Self::unpack_token_account(token_b_info, &token_swap.token_program_id())?;
        let pool_mint = Self::unpack_mint(pool_mint_info, &token_swap.token_program_id())?;
        let pool_token_amount = to_u128(pool_token_amount)?;
        let pool_mint_supply = to_u128(pool_mint.supply)?;

        let results = calculator
            .pool_tokens_to_trading_tokens(
                pool_token_amount,
                pool_mint_supply,
                to_u128(token_a.amount)?,
                to_u128(token_b.amount)?,
                RoundDirection::Ceiling,
            )
            .ok_or(SwapError::ZeroTradingTokens)?;
        let token_a_amount = to_u64(results.token_a_amount)?;
        if token_a_amount > maximum_token_a_amount {
            return Err(SwapError::ExceededSlippage.into());
        }
        if token_a_amount == 0 {
            return Err(SwapError::ZeroTradingTokens.into());
        }
        let token_b_amount = to_u64(results.token_b_amount)?;
        if token_b_amount > maximum_token_b_amount {
            return Err(SwapError::ExceededSlippage.into());
        }
        if token_b_amount == 0 {
            return Err(SwapError::ZeroTradingTokens.into());
        }

        let pool_token_amount = to_u64(pool_token_amount)?;

        Self::token_transfer(
            swap_info.key,
            token_program_info.clone(),
            source_a_info.clone(),
            token_a_info.clone(),
            user_transfer_authority_info.clone(),
            token_swap.nonce(),
            token_a_amount,
        )?;
        Self::token_transfer(
            swap_info.key,
            token_program_info.clone(),
            source_b_info.clone(),
            token_b_info.clone(),
            user_transfer_authority_info.clone(),
            token_swap.nonce(),
            token_b_amount,
        )?;
        Self::token_mint_to(
            swap_info.key,
            token_program_info.clone(),
            pool_mint_info.clone(),
            dest_info.clone(),
            authority_info.clone(),
            token_swap.nonce(),
            pool_token_amount,
        )?;

        Ok(())
    }

    /// Processes an [WithdrawAllTokenTypes](enum.Instruction.html).
    pub fn process_withdraw_all_token_types(
        program_id: &Pubkey,
        pool_token_amount: u64,
        minimum_token_a_amount: u64,
        minimum_token_b_amount: u64,
        accounts: &[AccountInfo],
    ) -> ProgramResult {
        let account_info_iter = &mut accounts.iter();
        let swap_info = next_account_info(account_info_iter)?;
        let authority_info = next_account_info(account_info_iter)?;
        let user_transfer_authority_info = next_account_info(account_info_iter)?;
        let pool_mint_info = next_account_info(account_info_iter)?;
        let source_info = next_account_info(account_info_iter)?;
        let token_a_info = next_account_info(account_info_iter)?;
        let token_b_info = next_account_info(account_info_iter)?;
        let dest_token_a_info = next_account_info(account_info_iter)?;
        let dest_token_b_info = next_account_info(account_info_iter)?;
        let pool_fee_account_info = next_account_info(account_info_iter)?;
        let token_program_info = next_account_info(account_info_iter)?;

        let token_swap = SwapVersion::unpack(&swap_info.data.borrow())?;
        Self::check_accounts(
            token_swap.as_ref(),
            program_id,
            swap_info,
            authority_info,
            token_a_info,
            token_b_info,
            pool_mint_info,
            token_program_info,
            Some(dest_token_a_info),
            Some(dest_token_b_info),
            Some(pool_fee_account_info),
        )?;

        let token_a = Self::unpack_token_account(token_a_info, token_swap.token_program_id())?;
        let token_b = Self::unpack_token_account(token_b_info, token_swap.token_program_id())?;
        let pool_mint = Self::unpack_mint(pool_mint_info, token_swap.token_program_id())?;

        let calculator = &token_swap.swap_curve().calculator;

        let withdraw_fee: u128 = if *pool_fee_account_info.key == *source_info.key {
            // withdrawing from the fee account, don't assess withdraw fee
            0
        } else {
            token_swap
                .fees()
                .owner_withdraw_fee(to_u128(pool_token_amount)?)
                .ok_or(SwapError::FeeCalculationFailure)?
        };
        let pool_token_amount = to_u128(pool_token_amount)?
            .checked_sub(withdraw_fee)
            .ok_or(SwapError::CalculationFailure)?;

        let results = calculator
            .pool_tokens_to_trading_tokens(
                pool_token_amount,
                to_u128(pool_mint.supply)?,
                to_u128(token_a.amount)?,
                to_u128(token_b.amount)?,
                RoundDirection::Floor,
            )
            .ok_or(SwapError::ZeroTradingTokens)?;
        let token_a_amount = to_u64(results.token_a_amount)?;
        if token_a_amount < minimum_token_a_amount {
            return Err(SwapError::ExceededSlippage.into());
        }
        if token_a_amount == 0 && token_a.amount != 0 {
            return Err(SwapError::ZeroTradingTokens.into());
        }
        let token_b_amount = to_u64(results.token_b_amount)?;
        if token_b_amount < minimum_token_b_amount {
            return Err(SwapError::ExceededSlippage.into());
        }
        if token_b_amount == 0 && token_b.amount != 0 {
            return Err(SwapError::ZeroTradingTokens.into());
        }

        if withdraw_fee > 0 {
            Self::token_transfer(
                swap_info.key,
                token_program_info.clone(),
                source_info.clone(),
                pool_fee_account_info.clone(),
                user_transfer_authority_info.clone(),
                token_swap.nonce(),
                to_u64(withdraw_fee)?,
            )?;
        }
        Self::token_burn(
            swap_info.key,
            token_program_info.clone(),
            source_info.clone(),
            pool_mint_info.clone(),
            user_transfer_authority_info.clone(),
            token_swap.nonce(),
            to_u64(pool_token_amount)?,
        )?;

        let token_a_amount = std::cmp::min(token_a.amount, token_a_amount);
        if token_a_amount > 0 {
            Self::token_transfer(
                swap_info.key,
                token_program_info.clone(),
                token_a_info.clone(),
                dest_token_a_info.clone(),
                authority_info.clone(),
                token_swap.nonce(),
                token_a_amount,
            )?;
        }
        let token_b_amount = std::cmp::min(token_b.amount, token_b_amount);
        if token_b_amount > 0 {
            Self::token_transfer(
                swap_info.key,
                token_program_info.clone(),
                token_b_info.clone(),
                dest_token_b_info.clone(),
                authority_info.clone(),
                token_swap.nonce(),
                token_b_amount,
            )?;
        }
        Ok(())
    }

    /// Processes DepositSingleTokenTypeExactAmountIn
    pub fn process_deposit_single_token_type_exact_amount_in(
        program_id: &Pubkey,
        source_token_amount: u64,
        minimum_pool_token_amount: u64,
        accounts: &[AccountInfo],
    ) -> ProgramResult {
        let account_info_iter = &mut accounts.iter();
        let swap_info = next_account_info(account_info_iter)?;
        let authority_info = next_account_info(account_info_iter)?;
        let user_transfer_authority_info = next_account_info(account_info_iter)?;
        let source_info = next_account_info(account_info_iter)?;
        let swap_token_a_info = next_account_info(account_info_iter)?;
        let swap_token_b_info = next_account_info(account_info_iter)?;
        let pool_mint_info = next_account_info(account_info_iter)?;
        let destination_info = next_account_info(account_info_iter)?;
        let token_program_info = next_account_info(account_info_iter)?;

        let token_swap = SwapVersion::unpack(&swap_info.data.borrow())?;
        let source_account =
            Self::unpack_token_account(source_info, &token_swap.token_program_id())?;
        let swap_token_a =
            Self::unpack_token_account(swap_token_a_info, &token_swap.token_program_id())?;
        let swap_token_b =
            Self::unpack_token_account(swap_token_b_info, &token_swap.token_program_id())?;

        let trade_direction = if source_account.mint == swap_token_a.mint {
            TradeDirection::AtoB
        } else if source_account.mint == swap_token_b.mint {
            TradeDirection::BtoA
        } else {
            return Err(SwapError::IncorrectSwapAccount.into());
        };

        let (source_a_info, source_b_info) = match trade_direction {
            TradeDirection::AtoB => (Some(source_info), None),
            TradeDirection::BtoA => (None, Some(source_info)),
        };

        Self::check_accounts(
            token_swap.as_ref(),
            program_id,
            swap_info,
            authority_info,
            swap_token_a_info,
            swap_token_b_info,
            pool_mint_info,
            token_program_info,
            source_a_info,
            source_b_info,
            None,
        )?;

        let pool_mint = Self::unpack_mint(pool_mint_info, &token_swap.token_program_id())?;
        let pool_mint_supply = to_u128(pool_mint.supply)?;

        let pool_token_amount = token_swap
            .swap_curve()
            .trading_tokens_to_pool_tokens(
                to_u128(source_token_amount)?,
                to_u128(swap_token_a.amount)?,
                to_u128(swap_token_b.amount)?,
                pool_mint_supply,
                trade_direction,
                RoundDirection::Floor,
                token_swap.fees(),
            )
            .ok_or(SwapError::ZeroTradingTokens)?;

        let pool_token_amount = to_u64(pool_token_amount)?;
        if pool_token_amount < minimum_pool_token_amount {
            return Err(SwapError::ExceededSlippage.into());
        }
        if pool_token_amount == 0 {
            return Err(SwapError::ZeroTradingTokens.into());
        }

        match trade_direction {
            TradeDirection::AtoB => {
                Self::token_transfer(
                    swap_info.key,
                    token_program_info.clone(),
                    source_info.clone(),
                    swap_token_a_info.clone(),
                    user_transfer_authority_info.clone(),
                    token_swap.nonce(),
                    source_token_amount,
                )?;
            }
            TradeDirection::BtoA => {
                Self::token_transfer(
                    swap_info.key,
                    token_program_info.clone(),
                    source_info.clone(),
                    swap_token_b_info.clone(),
                    user_transfer_authority_info.clone(),
                    token_swap.nonce(),
                    source_token_amount,
                )?;
            }
        }
        Self::token_mint_to(
            swap_info.key,
            token_program_info.clone(),
            pool_mint_info.clone(),
            destination_info.clone(),
            authority_info.clone(),
            token_swap.nonce(),
            pool_token_amount,
        )?;

        Ok(())
    }

    /// Processes a [WithdrawSingleTokenTypeExactAmountOut](enum.Instruction.html).
    pub fn process_withdraw_single_token_type_exact_amount_out(
        program_id: &Pubkey,
        destination_token_amount: u64,
        maximum_pool_token_amount: u64,
        accounts: &[AccountInfo],
    ) -> ProgramResult {
        let account_info_iter = &mut accounts.iter();
        let swap_info = next_account_info(account_info_iter)?;
        let authority_info = next_account_info(account_info_iter)?;
        let user_transfer_authority_info = next_account_info(account_info_iter)?;
        let pool_mint_info = next_account_info(account_info_iter)?;
        let source_info = next_account_info(account_info_iter)?;
        let swap_token_a_info = next_account_info(account_info_iter)?;
        let swap_token_b_info = next_account_info(account_info_iter)?;
        let destination_info = next_account_info(account_info_iter)?;
        let pool_fee_account_info = next_account_info(account_info_iter)?;
        let token_program_info = next_account_info(account_info_iter)?;

        let token_swap = SwapVersion::unpack(&swap_info.data.borrow())?;
        let destination_account =
            Self::unpack_token_account(destination_info, &token_swap.token_program_id())?;
        let swap_token_a =
            Self::unpack_token_account(swap_token_a_info, &token_swap.token_program_id())?;
        let swap_token_b =
            Self::unpack_token_account(swap_token_b_info, &token_swap.token_program_id())?;

        let trade_direction = if destination_account.mint == swap_token_a.mint {
            TradeDirection::AtoB
        } else if destination_account.mint == swap_token_b.mint {
            TradeDirection::BtoA
        } else {
            return Err(SwapError::IncorrectSwapAccount.into());
        };

        let (destination_a_info, destination_b_info) = match trade_direction {
            TradeDirection::AtoB => (Some(destination_info), None),
            TradeDirection::BtoA => (None, Some(destination_info)),
        };
        Self::check_accounts(
            token_swap.as_ref(),
            program_id,
            swap_info,
            authority_info,
            swap_token_a_info,
            swap_token_b_info,
            pool_mint_info,
            token_program_info,
            destination_a_info,
            destination_b_info,
            Some(pool_fee_account_info),
        )?;

        let pool_mint = Self::unpack_mint(pool_mint_info, &token_swap.token_program_id())?;
        let pool_mint_supply = to_u128(pool_mint.supply)?;
        let (swap_token_a_amount, swap_token_b_amount) = match trade_direction {
            TradeDirection::AtoB => (
                to_u128(
                    swap_token_a
                        .amount
                        .checked_sub(destination_token_amount)
                        .ok_or(SwapError::CalculationFailure)?,
                )?,
                to_u128(swap_token_b.amount)?,
            ),
            TradeDirection::BtoA => (
                to_u128(swap_token_a.amount)?,
                to_u128(
                    swap_token_b
                        .amount
                        .checked_sub(destination_token_amount)
                        .ok_or(SwapError::CalculationFailure)?,
                )?,
            ),
        };

        let burn_pool_token_amount = token_swap
            .swap_curve()
            .trading_tokens_to_pool_tokens(
                to_u128(destination_token_amount)?,
                swap_token_a_amount,
                swap_token_b_amount,
                pool_mint_supply,
                trade_direction,
                RoundDirection::Ceiling,
                token_swap.fees(),
            )
            .ok_or(SwapError::ZeroTradingTokens)?;

        let withdraw_fee: u128 = if *pool_fee_account_info.key == *source_info.key {
            // withdrawing from the fee account, don't assess withdraw fee
            0
        } else {
            token_swap
                .fees()
                .owner_withdraw_fee(burn_pool_token_amount)
                .ok_or(SwapError::FeeCalculationFailure)?
        };
        let pool_token_amount = burn_pool_token_amount
            .checked_add(withdraw_fee)
            .ok_or(SwapError::CalculationFailure)?;
        if to_u64(pool_token_amount)? > maximum_pool_token_amount {
            return Err(SwapError::ExceededSlippage.into());
        }
        if pool_token_amount == 0 {
            return Err(SwapError::ZeroTradingTokens.into());
        }

        if withdraw_fee > 0 {
            Self::token_transfer(
                swap_info.key,
                token_program_info.clone(),
                source_info.clone(),
                pool_fee_account_info.clone(),
                user_transfer_authority_info.clone(),
                token_swap.nonce(),
                to_u64(withdraw_fee)?,
            )?;
        }
        Self::token_burn(
            swap_info.key,
            token_program_info.clone(),
            source_info.clone(),
            pool_mint_info.clone(),
            user_transfer_authority_info.clone(),
            token_swap.nonce(),
            to_u64(burn_pool_token_amount)?,
        )?;
        match trade_direction {
            TradeDirection::AtoB => {
                Self::token_transfer(
                    swap_info.key,
                    token_program_info.clone(),
                    swap_token_a_info.clone(),
                    destination_info.clone(),
                    authority_info.clone(),
                    token_swap.nonce(),
                    destination_token_amount,
                )?;
            }
            TradeDirection::BtoA => {
                Self::token_transfer(
                    swap_info.key,
                    token_program_info.clone(),
                    swap_token_b_info.clone(),
                    destination_info.clone(),
                    authority_info.clone(),
                    token_swap.nonce(),
                    destination_token_amount,
                )?;
            }
        }

        Ok(())
    }

    /// Processes a [StartFarming](enum.Instruction.html).
    pub fn process_start_farming(
        program_id: &Pubkey,
        pool_token_amount: u64,
        accounts: &[AccountInfo],
    ) -> ProgramResult {
        let account_info_iter = &mut accounts.iter();
        let swap_info = next_account_info(account_info_iter)?;
        let farming_state_info = next_account_info(account_info_iter)?;
        let farming_ticket_info = next_account_info(account_info_iter)?;
        let farming_token_freeze_info = next_account_info(account_info_iter)?;
        let user_token_info = next_account_info(account_info_iter)?;
        let user_transfer_authority_info = next_account_info(account_info_iter)?;
        let user_key = next_account_info(account_info_iter)?;

        let token_program_info = next_account_info(account_info_iter)?;
        let clock_info = next_account_info(account_info_iter)?;

        let clock = &Clock::from_account_info(clock_info)?;
        let token_swap = SwapVersion::unpack(&swap_info.data.borrow())?;
        let farming_state = FarmingState::unpack(&farming_state_info.data.borrow())?;
        if *clock_info.key != solana_program::sysvar::clock::ID {
            return Err(ProgramError::InvalidAccountData);
        }

        Self::check_farming_accounts(
            program_id,
            token_swap.as_ref(),
            swap_info,
            &farming_state,
            farming_state_info,
            Some(token_program_info),
            Some(farming_token_freeze_info),
            None,
            None,
            None,
            None,
            None,
        )?;
        if FarmingTicket::is_initialized(&farming_ticket_info.data.borrow()) {
            return Err(SwapError::AlreadyInUse.into());
        }

        if !user_key.is_signer {
            return Err(ProgramError::MissingRequiredSignature);
        }

        Self::token_transfer(
            swap_info.key,
            token_program_info.clone(),
            user_token_info.clone(),
            farming_token_freeze_info.clone(),
            user_transfer_authority_info.clone(),
            token_swap.nonce(),
            pool_token_amount,
        )?;
        let obj = FarmingTicket {
            discriminator: u64::from_le_bytes(TICKET_DISCRIMINATOR),
            is_initialized: true,
            tokens_frozen: pool_token_amount,
            start_time: clock.unix_timestamp,
            end_time: UnixTimestamp::MAX,
            token_authority: user_key.key.clone(),
            farming_state: farming_state_info.key.clone(),
        };

        FarmingTicket::pack(obj, &mut farming_ticket_info.data.borrow_mut())?;
        Ok(())
    }

    /// Processes an [WithdrawFarmed](enum.Instruction.html).
    pub fn process_withdraw_farmed(
        program_id: &Pubkey,
        accounts: &[AccountInfo],
    ) -> ProgramResult {
        let account_info_iter = &mut accounts.iter();
        let swap_info = next_account_info(account_info_iter)?;
        let farming_info = next_account_info(account_info_iter)?;
        let farming_ticket_info = next_account_info(account_info_iter)?;
        let farming_token_info = next_account_info(account_info_iter)?;
        let swap_authority_info = next_account_info(account_info_iter)?;
        let user_token_info = next_account_info(account_info_iter)?;
        let user_transfer_authority_info = next_account_info(account_info_iter)?;
        let clock_info = next_account_info(account_info_iter)?;
        let token_program_info = next_account_info(account_info_iter)?;

        let clock = &Clock::from_account_info(clock_info)?;
        let token_swap = SwapVersion::unpack(&swap_info.data.borrow())?;
        let mut farming_ticket = FarmingTicket::unpack(&farming_ticket_info.data.borrow())?;
        let farming_state = FarmingState::unpack(&farming_info.data.borrow())?;

        if *clock_info.key != solana_program::sysvar::clock::ID {
            return Err(ProgramError::InvalidAccountData);
        }

        if !farming_ticket.is_initialized || !farming_state.is_initialized {
            return Err(ProgramError::UninitializedAccount.into());
        }
        Self::check_farming_accounts(
            program_id,
            token_swap.as_ref(),
            swap_info,
            &farming_state,
            farming_info,
            Some(token_program_info),
            None,
            Some(farming_token_info),
            Some(&farming_ticket),
            Some(farming_ticket_info),
            Some(user_transfer_authority_info),
            Some(swap_authority_info),
        )?;
        if !user_transfer_authority_info.is_signer {
            return Err(ProgramError::MissingRequiredSignature);
        }

        if !farming_state.is_no_withdrawal_period_passed(clock.unix_timestamp) {
            return Err(FarmingError::MinimumWithdrawalTimeNotPassed.into());
        }

        let (tokens_to_withdraw, timestamp) = farming_state
            .calculate_withdraw_tokens(&farming_ticket)
            .ok_or(FarmingError::FarmingTokenCalculationError)?;
        if tokens_to_withdraw == 0 || timestamp == farming_ticket.start_time {
            return Err(FarmingError::NoTokensToWithdraw.into());
        }
        farming_ticket.start_time = timestamp;

        let tokens_to_withdraw = to_u64(tokens_to_withdraw)?;

        Self::token_transfer(
            swap_info.key,
            token_program_info.clone(),
            farming_token_info.clone(),
            user_token_info.clone(),
            swap_authority_info.clone(),
            token_swap.nonce(),
            tokens_to_withdraw,
        )?;

        if farming_ticket.end_time != UnixTimestamp::MAX {
            farming_ticket = FarmingTicket::default();
        }

        FarmingTicket::pack(farming_ticket, &mut farming_ticket_info.data.borrow_mut())?;

        Ok(())
    }

    /// Processes an [EndFarming](enum.Instruction.html).
    pub fn process_end_farming(
        program_id: &Pubkey,
        accounts: &[AccountInfo],
    ) -> ProgramResult {
        let account_info_iter = &mut accounts.iter();
        let swap_info = next_account_info(account_info_iter)?;
        let farming_info = next_account_info(account_info_iter)?;
        let farming_ticket_info = next_account_info(account_info_iter)?;
        let pool_token_freeze_info = next_account_info(account_info_iter)?;
        let swap_authority_info = next_account_info(account_info_iter)?;
        let user_pool_token_info = next_account_info(account_info_iter)?;
        let user_transfer_authority_info = next_account_info(account_info_iter)?;
        let clock_info = next_account_info(account_info_iter)?;
        let token_program_info = next_account_info(account_info_iter)?;

        let clock = &Clock::from_account_info(clock_info)?;
        let token_swap = SwapVersion::unpack(&swap_info.data.borrow())?;
        let mut farming_ticket = FarmingTicket::unpack(&farming_ticket_info.data.borrow())?;
        let farming_state = FarmingState::unpack(&farming_info.data.borrow())?;

        if *clock_info.key != solana_program::sysvar::clock::ID {
            return Err(ProgramError::InvalidAccountData);
        }

        if !farming_ticket.is_initialized || !farming_state.is_initialized {
            return Err(ProgramError::UninitializedAccount.into());
        }

        Self::check_farming_accounts(
            program_id,
            token_swap.as_ref(),
            swap_info,
            &farming_state,
            farming_info,
            Some(token_program_info),
            Some(pool_token_freeze_info),
            None,
            Some(&farming_ticket),
            Some(farming_ticket_info),
            Some(user_transfer_authority_info),
            Some(swap_authority_info),
        )?;
        if !user_transfer_authority_info.is_signer {
            return Err(ProgramError::MissingRequiredSignature);
        }

        Self::token_transfer(
            swap_info.key,
            token_program_info.clone(),
            pool_token_freeze_info.clone(),
            user_pool_token_info.clone(),
            swap_authority_info.clone(),
            token_swap.nonce(),
            farming_ticket.tokens_frozen,
        )?;

        farming_ticket.end_time = clock.unix_timestamp;

        FarmingTicket::pack(farming_ticket, &mut farming_ticket_info.data.borrow_mut())?;

        Ok(())
    }

    /// Processes an [EndFarming](enum.Instruction.html).
    pub fn process_initialize_farming(
        program_id: &Pubkey,
        token_amount: u64,
        tokens_per_period: u64,
        period_length: u64,
        accounts: &[AccountInfo],
    ) -> ProgramResult {
        let account_info_iter = &mut accounts.iter();
        let swap_info = next_account_info(account_info_iter)?;
        let farming_info = next_account_info(account_info_iter)?;
        let farming_token_info = next_account_info(account_info_iter)?;
        let user_farming_token_info = next_account_info(account_info_iter)?;
        let user_transfer_authority_info = next_account_info(account_info_iter)?;
        let fee_account = next_account_info(account_info_iter)?;
        let fee_authority = next_account_info(account_info_iter)?;
        let authority_info = next_account_info(account_info_iter)?;
        let clock_info = next_account_info(account_info_iter)?;
        let token_program_info = next_account_info(account_info_iter)?;

        let clock = &Clock::from_account_info(clock_info)?;
        let token_swap = SwapVersion::unpack(&swap_info.data.borrow())?;

        let farming_token =
            Self::unpack_token_account(farming_token_info, &token_swap.token_program_id())?;

        let fee_token =
            Self::unpack_token_account(fee_account, &token_swap.token_program_id())?;


        if *clock_info.key != solana_program::sysvar::clock::ID {
            return Err(ProgramError::InvalidAccountData);
        }

        if swap_info.owner != program_id {
            return Err(ProgramError::IncorrectProgramId);
        }

        if !fee_authority.is_signer {
            return Err(ProgramError::MissingRequiredSignature);
        }

        if *fee_account.key != *token_swap.pool_fee_account() ||
            fee_token.owner != *fee_authority.key {
            return Err(ProgramError::InvalidAccountData);
        }

        if *authority_info.key != farming_token.owner {
            return Err(SwapError::InvalidOwner.into());
        }

        if *token_swap.farming_state() != *farming_info.key {
            return Err(ProgramError::InvalidAccountData.into());
        }

        if FarmingState::is_initialized(&farming_info.data.borrow())? {
            return Err(SwapError::AlreadyInUse.into());
        }

        Self::token_transfer(
            swap_info.key,
            token_program_info.clone(),
            user_farming_token_info.clone(),
            farming_token_info.clone(),
            user_transfer_authority_info.clone(),
            token_swap.nonce(),
            token_amount,
        )?;

        let obj = FarmingState {
            discriminator: u64::from_le_bytes(FARMING_STATE_DISCRIMINATOR),
            is_initialized: true,
            tokens_unlocked: 0,
            tokens_total: token_amount,
            tokens_per_period,
            period_length,
            start_time: clock.unix_timestamp,
            current_time: clock.unix_timestamp,
            attached_swap_account: *swap_info.key,
            farming_token_account: *farming_token_info.key,
            farming_snapshots: Default::default(),
        };

        FarmingState::pack(obj, &mut farming_info.data.borrow_mut())?;

        Ok(())
    }

    /// Processes a [TakeFarmingSnapshot](enum.Instruction.html).
    pub fn process_take_farming_snapshot(
        program_id: &Pubkey,
        accounts: &[AccountInfo],
    ) -> ProgramResult {
        let account_info_iter = &mut accounts.iter();
        let swap_info = next_account_info(account_info_iter)?;
        let farming_info = next_account_info(account_info_iter)?;
        let token_freeze_account_info = next_account_info(account_info_iter)?;
        let fee_account = next_account_info(account_info_iter)?;
        let fee_authority = next_account_info(account_info_iter)?;
        let clock_info = next_account_info(account_info_iter)?;

        let clock = &Clock::from_account_info(clock_info)?;
        let token_swap = SwapVersion::unpack(&swap_info.data.borrow())?;
        let farming_state = FarmingState::unpack(&farming_info.data.borrow())?;
        let token_freeze_account = spl_token::state::Account::unpack(&token_freeze_account_info.data.borrow())?;

        let fee_token =
            Self::unpack_token_account(fee_account, &token_swap.token_program_id())?;

        if *clock_info.key != solana_program::sysvar::clock::ID {
            return Err(ProgramError::InvalidAccountData);
        }

        if swap_info.owner != program_id {
            return Err(ProgramError::IncorrectProgramId);
        }

        if !fee_authority.is_signer {
            return Err(ProgramError::MissingRequiredSignature);
        }

        if *fee_account.key != *token_swap.pool_fee_account() ||
            fee_token.owner != *fee_authority.key {
            return Err(ProgramError::InvalidAccountData);
        }

        Self::check_farming_accounts(
            program_id,
            token_swap.as_ref(),
            swap_info,
            &farming_state,
            farming_info,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )?;

        if *token_swap.farming_state() != *farming_info.key {
            return Err(ProgramError::InvalidAccountData.into());
        }

        if token_freeze_account.amount == 0 {
            return Err(FarmingError::CannotSnapshotNoTokensFrozen.into());
        }

        let mut farming_state = FarmingState::unpack_from_slice(&farming_info.data.borrow_mut())?;

        let mut last_snapshot_time = farming_state.start_time;
        let last_index = farming_state.farming_snapshots.next_index as usize;
        if last_index > 0 {
            last_snapshot_time = farming_state.farming_snapshots.snapshots[last_index - 1].time;
        }

        let periods_passed = ((clock.unix_timestamp - last_snapshot_time) as u64) / farming_state.period_length;
        if periods_passed == 0 {
            return Err(FarmingError::CannotSnapshotNoTokensToUnlock.into());
        }

        farming_state.current_time = last_snapshot_time + (periods_passed * farming_state.period_length) as i64;

        let tokens_to_unlock = periods_passed * farming_state.tokens_per_period;


        let last_tokens_unlocked = farming_state.tokens_unlocked;
        farming_state.tokens_unlocked = farming_state.tokens_unlocked
            .checked_add(tokens_to_unlock)
            .ok_or(FarmingError::FarmingTokenCalculationError)?;
        if farming_state.tokens_unlocked > farming_state.tokens_total {
            farming_state.tokens_unlocked = farming_state.tokens_total;
        }
        if last_tokens_unlocked == farming_state.tokens_unlocked {
            return Err(FarmingError::CannotSnapshotNoTokensToUnlock.into());
        }

        let index = farming_state.farming_snapshots.next_index as usize;

        if index == QUEUE_LENGTH {
            return Err(ProgramError::AccountDataTooSmall);
        }

        farming_state.farming_snapshots.snapshots.push(Snapshot {
            is_initialized: true,
            tokens_frozen: token_freeze_account.amount,
            farming_tokens: farming_state.tokens_unlocked,
            time: clock.unix_timestamp,
        });
        farming_state.farming_snapshots.next_index = (index + 1) as u64;

        FarmingState::pack(farming_state, &mut farming_info.data.borrow_mut())?;

        Ok(())
    }

    /// Processes an [Instruction](enum.Instruction.html).
    pub fn process(program_id: &Pubkey, accounts: &[AccountInfo], input: &[u8]) -> ProgramResult {
        Self::process_with_constraints(program_id, accounts, input, &SWAP_CONSTRAINTS)
    }

    /// Processes an instruction given extra constraint
    pub fn process_with_constraints(
        program_id: &Pubkey,
        accounts: &[AccountInfo],
        input: &[u8],
        swap_constraints: &Option<SwapConstraints>,
    ) -> ProgramResult {
        let instruction = SwapInstruction::unpack(input)?;
        match instruction {
            SwapInstruction::Initialize(Initialize {
                                            nonce,
                                            fees,
                                            swap_curve,
                                        }) => {
                msg!("Instruction: Init");
                Self::process_initialize(
                    program_id,
                    nonce,
                    fees,
                    swap_curve,
                    accounts,
                    swap_constraints,
                )
            }
            SwapInstruction::Swap(Swap {
                                      amount_in,
                                      minimum_amount_out,
                                  }) => {
                msg!("Instruction: Swap");
                Self::process_swap(program_id, amount_in, minimum_amount_out, accounts)
            }
            SwapInstruction::DepositAllTokenTypes(DepositAllTokenTypes {
                                                      pool_token_amount,
                                                      maximum_token_a_amount,
                                                      maximum_token_b_amount,
                                                  }) => {
                msg!("Instruction: DepositAllTokenTypes");
                Self::process_deposit_all_token_types(
                    program_id,
                    pool_token_amount,
                    maximum_token_a_amount,
                    maximum_token_b_amount,
                    accounts,
                )
            }
            SwapInstruction::WithdrawAllTokenTypes(WithdrawAllTokenTypes {
                                                       pool_token_amount,
                                                       minimum_token_a_amount,
                                                       minimum_token_b_amount,
                                                   }) => {
                msg!("Instruction: WithdrawAllTokenTypes");
                Self::process_withdraw_all_token_types(
                    program_id,
                    pool_token_amount,
                    minimum_token_a_amount,
                    minimum_token_b_amount,
                    accounts,
                )
            }
            SwapInstruction::DepositSingleTokenTypeExactAmountIn(
                DepositSingleTokenTypeExactAmountIn {
                    source_token_amount,
                    minimum_pool_token_amount,
                },
            ) => {
                msg!("Instruction: DepositSingleTokenTypeExactAmountIn");
                Self::process_deposit_single_token_type_exact_amount_in(
                    program_id,
                    source_token_amount,
                    minimum_pool_token_amount,
                    accounts,
                )
            }
            SwapInstruction::WithdrawSingleTokenTypeExactAmountOut(
                WithdrawSingleTokenTypeExactAmountOut {
                    destination_token_amount,
                    maximum_pool_token_amount,
                },
            ) => {
                msg!("Instruction: WithdrawSingleTokenTypeExactAmountOut");
                Self::process_withdraw_single_token_type_exact_amount_out(
                    program_id,
                    destination_token_amount,
                    maximum_pool_token_amount,
                    accounts,
                )
            }
            SwapInstruction::StartFarming(
                StartFarming {
                    pool_token_amount,
                },
            ) => {
                msg!("Instruction: StartFarming");
                Self::process_start_farming(
                    program_id,
                    pool_token_amount,
                    accounts,
                )
            }
            SwapInstruction::WithdrawFarmed => {
                msg!("Instruction: WithdrawFarmed");
                Self::process_withdraw_farmed(
                    program_id,
                    accounts,
                )
            }
            SwapInstruction::EndFarming => {
                msg!("Instruction: EndFarming");
                Self::process_end_farming(
                    program_id,
                    accounts,
                )
            }
            SwapInstruction::InitializeFarming(
                InitializeFarming {
                    token_amount,
                    tokens_per_period,
                    period_length,
                },
            ) => {
                msg!("Instruction: InitializeFarming");
                Self::process_initialize_farming(
                    program_id,
                    token_amount,
                    tokens_per_period,
                    period_length,
                    accounts,
                )
            }
            SwapInstruction::TakeFarmingSnapshot => {
                msg!("Instruction: TakeFarmingSnapshot");
                Self::process_take_farming_snapshot(
                    program_id,
                    accounts,
                )
            }
        }
    }
}

impl PrintProgramError for SwapError {
    fn print<E>(&self)
        where
            E: 'static + std::error::Error + DecodeError<E> + PrintProgramError + FromPrimitive,
    {
        match self {
            SwapError::AlreadyInUse => msg!("Error: Swap account already in use"),
            SwapError::InvalidProgramAddress => {
                msg!("Error: Invalid program address generated from nonce and key")
            }
            SwapError::InvalidOwner => {
                msg!("Error: The input account owner is not the program address")
            }
            SwapError::InvalidOutputOwner => {
                msg!("Error: Output pool account owner cannot be the program address")
            }
            SwapError::ExpectedMint => msg!("Error: Deserialized account is not an SPL Token mint"),
            SwapError::ExpectedAccount => {
                msg!("Error: Deserialized account is not an SPL Token account")
            }
            SwapError::EmptySupply => msg!("Error: Input token account empty"),
            SwapError::InvalidSupply => msg!("Error: Pool token mint has a non-zero supply"),
            SwapError::RepeatedMint => msg!("Error: Swap input token accounts have the same mint"),
            SwapError::InvalidDelegate => msg!("Error: Token account has a delegate"),
            SwapError::InvalidInput => msg!("Error: InvalidInput"),
            SwapError::IncorrectSwapAccount => {
                msg!("Error: Address of the provided swap token account is incorrect")
            }
            SwapError::IncorrectPoolMint => {
                msg!("Error: Address of the provided pool token mint is incorrect")
            }
            SwapError::InvalidOutput => msg!("Error: InvalidOutput"),
            SwapError::CalculationFailure => msg!("Error: CalculationFailure"),
            SwapError::InvalidInstruction => msg!("Error: InvalidInstruction"),
            SwapError::ExceededSlippage => {
                msg!("Error: Swap instruction exceeds desired slippage limit")
            }
            SwapError::InvalidCloseAuthority => msg!("Error: Token account has a close authority"),
            SwapError::InvalidFreezeAuthority => {
                msg!("Error: Pool token mint has a freeze authority")
            }
            SwapError::IncorrectFeeAccount => msg!("Error: Pool fee token account incorrect"),
            SwapError::ZeroTradingTokens => {
                msg!("Error: Given pool token amount results in zero trading tokens")
            }
            SwapError::FeeCalculationFailure => msg!(
                "Error: The fee calculation failed due to overflow, underflow, or unexpected 0"
            ),
            SwapError::ConversionFailure => msg!("Error: Conversion to or from u64 failed."),
            SwapError::InvalidFee => {
                msg!("Error: The provided fee does not match the program owner's constraints")
            }
            SwapError::IncorrectTokenProgramId => {
                msg!("Error: The provided token program does not match the token program expected by the swap")
            }
            SwapError::UnsupportedCurveType => {
                msg!("Error: The provided curve type is not supported by the program owner")
            }
            SwapError::InvalidCurve => {
                msg!("Error: The provided curve parameters are invalid")
            }
            SwapError::UnsupportedCurveOperation => {
                msg!("Error: The operation cannot be performed on the given curve")
            }
        }
    }
}

fn to_u128(val: u64) -> Result<u128, SwapError> {
    val.try_into().map_err(|_| SwapError::ConversionFailure)
}

fn to_u64(val: u128) -> Result<u64, SwapError> {
    val.try_into().map_err(|_| SwapError::ConversionFailure)
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;
    use std::time::{SystemTime, UNIX_EPOCH};

    use solana_program::{instruction::Instruction, program_stubs, rent::Rent};
    use solana_sdk::account::{Account, create_account_for_test, create_is_signer_account_infos};
    use spl_token::{
        error::TokenError,
        instruction::{
            approve, AuthorityType, initialize_account, initialize_mint, mint_to, revoke,
            set_authority,
        },
    };

    use crate::{
        curve::{
            base::CurveType, constant_product::ConstantProductCurve,
        },
        curve::calculator::{CurveCalculator, INITIAL_SWAP_POOL_AMOUNT},
        instruction::{
            deposit_all_token_types, deposit_single_token_type_exact_amount_in, initialize, swap,
            withdraw_all_token_types, withdraw_single_token_type_exact_amount_out,
        },
    };
    use crate::instruction::{end_farming, initialize_farming, start_farming, take_farming_snapshot, withdraw_farmed};

    use super::*;

    // Test program id for the swap program.
    const SWAP_PROGRAM_ID: Pubkey = Pubkey::new_from_array([2u8; 32]);
    // Test program id for the token program.
    const TOKEN_PROGRAM_ID: Pubkey = Pubkey::new_from_array([1u8; 32]);

    struct TestSyscallStubs {}

    impl program_stubs::SyscallStubs for TestSyscallStubs {
        fn sol_invoke_signed(
            &self,
            instruction: &Instruction,
            account_infos: &[AccountInfo],
            signers_seeds: &[&[&[u8]]],
        ) -> ProgramResult {
            msg!("TestSyscallStubs::sol_invoke_signed()");

            let mut new_account_infos = vec![];

            // mimic check for token program in accounts
            if !account_infos.iter().any(|x| *x.key == TOKEN_PROGRAM_ID) {
                return Err(ProgramError::InvalidAccountData);
            }

            for meta in instruction.accounts.iter() {
                for account_info in account_infos.iter() {
                    if meta.pubkey == *account_info.key {
                        let mut new_account_info = account_info.clone();
                        for seeds in signers_seeds.iter() {
                            let signer =
                                Pubkey::create_program_address(&seeds, &SWAP_PROGRAM_ID).unwrap();
                            if *account_info.key == signer {
                                new_account_info.is_signer = true;
                            }
                        }
                        new_account_infos.push(new_account_info);
                    }
                }
            }

            spl_token::processor::Processor::process(
                &instruction.program_id,
                &new_account_infos,
                &instruction.data,
            )
        }
    }

    fn test_syscall_stubs() {
        use std::sync::Once;
        static ONCE: Once = Once::new();

        ONCE.call_once(|| {
            program_stubs::set_syscall_stubs(Box::new(TestSyscallStubs {}));
        });
    }

    struct SwapAccountInfo {
        nonce: u8,
        authority_key: Pubkey,
        fees: Fees,
        swap_curve: SwapCurve,
        swap_key: Pubkey,
        swap_account: Account,
        pool_mint_key: Pubkey,
        pool_mint_account: Account,
        pool_fee_key: Pubkey,
        pool_fee_account: Account,
        pool_token_key: Pubkey,
        pool_token_account: Account,
        token_a_key: Pubkey,
        token_a_account: Account,
        token_a_mint_key: Pubkey,
        token_a_mint_account: Account,
        token_b_key: Pubkey,
        token_b_account: Account,
        token_b_mint_key: Pubkey,
        token_b_mint_account: Account,
        token_freeze_account: Account,
        token_freeze_key: Pubkey,
        farming_state_key: Pubkey,
        farming_state_account: Account,
    }

    impl SwapAccountInfo {
        pub fn new(
            user_key: &Pubkey,
            fees: Fees,
            swap_curve: SwapCurve,
            token_a_amount: u64,
            token_b_amount: u64,
        ) -> Self {
            let swap_key = Pubkey::new_unique();
            let swap_account = Account::new(0, SwapVersion::LATEST_LEN, &SWAP_PROGRAM_ID);
            let (authority_key, nonce) =
                Pubkey::find_program_address(&[&swap_key.to_bytes()[..]], &SWAP_PROGRAM_ID);

            let (pool_mint_key, mut pool_mint_account) =
                create_mint(&TOKEN_PROGRAM_ID, &authority_key, None);
            let (pool_token_key, pool_token_account) = mint_token(
                &TOKEN_PROGRAM_ID,
                &pool_mint_key,
                &mut pool_mint_account,
                &authority_key,
                &user_key,
                0,
            );
            let mut constrained_fee_key = *user_key;
            if let Some(constraints) = crate::constraints::SWAP_CONSTRAINTS {
                constrained_fee_key = Pubkey::from_str(constraints.owner_key).expect("cannot create pubkey");
            }
            let (pool_fee_key, pool_fee_account) = mint_token(
                &TOKEN_PROGRAM_ID,
                &pool_mint_key,
                &mut pool_mint_account,
                &authority_key,
                &constrained_fee_key,
                0,
            );
            let (token_a_mint_key, mut token_a_mint_account) =
                create_mint(&TOKEN_PROGRAM_ID, &user_key, None);
            let (token_a_key, token_a_account) = mint_token(
                &TOKEN_PROGRAM_ID,
                &token_a_mint_key,
                &mut token_a_mint_account,
                &user_key,
                &authority_key,
                token_a_amount,
            );
            let (token_b_mint_key, mut token_b_mint_account) =
                create_mint(&TOKEN_PROGRAM_ID, &user_key, None);
            let (token_b_key, token_b_account) = mint_token(
                &TOKEN_PROGRAM_ID,
                &token_b_mint_key,
                &mut token_b_mint_account,
                &user_key,
                &authority_key,
                token_b_amount,
            );

            let (token_freeze_key, token_freeze_account) = mint_token(
                &TOKEN_PROGRAM_ID,
                &pool_mint_key,
                &mut pool_mint_account,
                &authority_key,
                &authority_key,
                0,
            );
            let farming_state_key = Pubkey::new_unique();
            let farming_state_account = Account::new(0, FarmingState::LEN, &SWAP_PROGRAM_ID);

            SwapAccountInfo {
                nonce,
                authority_key,
                fees,
                swap_curve,
                swap_key,
                swap_account,
                pool_mint_key,
                pool_mint_account,
                pool_fee_key,
                pool_fee_account,
                pool_token_key,
                pool_token_account,
                token_a_key,
                token_a_account,
                token_a_mint_key,
                token_a_mint_account,
                token_b_key,
                token_b_account,
                token_b_mint_key,
                token_b_mint_account,
                token_freeze_account,
                token_freeze_key,
                farming_state_key,
                farming_state_account,
            }
        }

        pub fn initialize_swap(&mut self) -> ProgramResult {
            do_process_instruction(
                initialize(
                    &SWAP_PROGRAM_ID,
                    &TOKEN_PROGRAM_ID,
                    &self.swap_key,
                    &self.authority_key,
                    &self.token_a_key,
                    &self.token_b_key,
                    &self.pool_mint_key,
                    &self.pool_fee_key,
                    &self.pool_token_key,
                    self.nonce,
                    self.fees.clone(),
                    self.swap_curve.clone(),
                    &self.farming_state_key,
                    &self.token_freeze_key,
                )
                    .unwrap(),
                vec![
                    &mut self.swap_account,
                    &mut Account::default(),
                    &mut self.token_a_account,
                    &mut self.token_b_account,
                    &mut self.pool_mint_account,
                    &mut self.pool_fee_account,
                    &mut self.pool_token_account,
                    &mut Account::default(),
                    &mut self.farming_state_account,
                    &mut self.token_freeze_account,
                ],
            )
        }

        pub fn setup_token_accounts(
            &mut self,
            mint_owner: &Pubkey,
            account_owner: &Pubkey,
            a_amount: u64,
            b_amount: u64,
            pool_amount: u64,
        ) -> (Pubkey, Account, Pubkey, Account, Pubkey, Account) {
            let (token_a_key, token_a_account) = mint_token(
                &TOKEN_PROGRAM_ID,
                &self.token_a_mint_key,
                &mut self.token_a_mint_account,
                &mint_owner,
                &account_owner,
                a_amount,
            );
            let (token_b_key, token_b_account) = mint_token(
                &TOKEN_PROGRAM_ID,
                &self.token_b_mint_key,
                &mut self.token_b_mint_account,
                &mint_owner,
                &account_owner,
                b_amount,
            );
            let (pool_key, pool_account) = mint_token(
                &TOKEN_PROGRAM_ID,
                &self.pool_mint_key,
                &mut self.pool_mint_account,
                &self.authority_key,
                &account_owner,
                pool_amount,
            );
            (
                token_a_key,
                token_a_account,
                token_b_key,
                token_b_account,
                pool_key,
                pool_account,
            )
        }

        fn get_token_account(&self, account_key: &Pubkey) -> &Account {
            if *account_key == self.token_a_key {
                return &self.token_a_account;
            } else if *account_key == self.token_b_key {
                return &self.token_b_account;
            }
            panic!("Could not find matching swap token account");
        }

        fn set_token_account(&mut self, account_key: &Pubkey, account: Account) {
            if *account_key == self.token_a_key {
                self.token_a_account = account;
                return;
            } else if *account_key == self.token_b_key {
                self.token_b_account = account;
                return;
            }
            panic!("Could not find matching swap token account");
        }

        #[allow(clippy::too_many_arguments)]
        pub fn swap(
            &mut self,
            user_key: &Pubkey,
            user_source_key: &Pubkey,
            mut user_source_account: &mut Account,
            swap_source_key: &Pubkey,
            swap_destination_key: &Pubkey,
            user_destination_key: &Pubkey,
            mut user_destination_account: &mut Account,
            amount_in: u64,
            minimum_amount_out: u64,
        ) -> ProgramResult {
            let user_transfer_key = Pubkey::new_unique();
            // approve moving from user source account
            do_process_instruction(
                approve(
                    &TOKEN_PROGRAM_ID,
                    &user_source_key,
                    &user_transfer_key,
                    &user_key,
                    &[],
                    amount_in,
                )
                    .unwrap(),
                vec![
                    &mut user_source_account,
                    &mut Account::default(),
                    &mut Account::default(),
                ],
            )
                .unwrap();

            let mut swap_source_account = self.get_token_account(swap_source_key).clone();
            let mut swap_destination_account = self.get_token_account(swap_destination_key).clone();

            // perform the swap
            do_process_instruction(
                swap(
                    &SWAP_PROGRAM_ID,
                    &TOKEN_PROGRAM_ID,
                    &self.swap_key,
                    &self.authority_key,
                    &user_transfer_key,
                    &user_source_key,
                    &swap_source_key,
                    &swap_destination_key,
                    &user_destination_key,
                    &self.pool_mint_key,
                    &self.pool_fee_key,
                    None,
                    Swap {
                        amount_in,
                        minimum_amount_out,
                    },
                )
                    .unwrap(),
                vec![
                    &mut self.swap_account,
                    &mut Account::default(),
                    &mut Account::default(),
                    &mut user_source_account,
                    &mut swap_source_account,
                    &mut swap_destination_account,
                    &mut user_destination_account,
                    &mut self.pool_mint_account,
                    &mut self.pool_fee_account,
                    &mut Account::default(),
                ],
            )?;

            self.set_token_account(swap_source_key, swap_source_account);
            self.set_token_account(swap_destination_key, swap_destination_account);

            Ok(())
        }

        #[allow(clippy::too_many_arguments)]
        pub fn deposit_all_token_types(
            &mut self,
            depositor_key: &Pubkey,
            depositor_token_a_key: &Pubkey,
            mut depositor_token_a_account: &mut Account,
            depositor_token_b_key: &Pubkey,
            mut depositor_token_b_account: &mut Account,
            depositor_pool_key: &Pubkey,
            mut depositor_pool_account: &mut Account,
            pool_token_amount: u64,
            maximum_token_a_amount: u64,
            maximum_token_b_amount: u64,
        ) -> ProgramResult {
            let user_transfer_authority = Pubkey::new_unique();
            do_process_instruction(
                approve(
                    &TOKEN_PROGRAM_ID,
                    &depositor_token_a_key,
                    &user_transfer_authority,
                    &depositor_key,
                    &[],
                    maximum_token_a_amount,
                )
                    .unwrap(),
                vec![
                    &mut depositor_token_a_account,
                    &mut Account::default(),
                    &mut Account::default(),
                ],
            )
                .unwrap();

            do_process_instruction(
                approve(
                    &TOKEN_PROGRAM_ID,
                    &depositor_token_b_key,
                    &user_transfer_authority,
                    &depositor_key,
                    &[],
                    maximum_token_b_amount,
                )
                    .unwrap(),
                vec![
                    &mut depositor_token_b_account,
                    &mut Account::default(),
                    &mut Account::default(),
                ],
            )
                .unwrap();

            do_process_instruction(
                deposit_all_token_types(
                    &SWAP_PROGRAM_ID,
                    &TOKEN_PROGRAM_ID,
                    &self.swap_key,
                    &self.authority_key,
                    &user_transfer_authority,
                    &depositor_token_a_key,
                    &depositor_token_b_key,
                    &self.token_a_key,
                    &self.token_b_key,
                    &self.pool_mint_key,
                    &depositor_pool_key,
                    DepositAllTokenTypes {
                        pool_token_amount,
                        maximum_token_a_amount,
                        maximum_token_b_amount,
                    },
                )
                    .unwrap(),
                vec![
                    &mut self.swap_account,
                    &mut Account::default(),
                    &mut Account::default(),
                    &mut depositor_token_a_account,
                    &mut depositor_token_b_account,
                    &mut self.token_a_account,
                    &mut self.token_b_account,
                    &mut self.pool_mint_account,
                    &mut depositor_pool_account,
                    &mut Account::default(),
                ],
            )
        }

        pub fn deposit_all_user_token_types(
            &mut self,
            user_info: &mut UserFarmingInfo,
            pool_token_amount: u64,
            maximum_token_a_amount: u64,
            maximum_token_b_amount: u64,
        ) -> ProgramResult {
            return self.deposit_all_token_types(
                &user_info.user_key,
                &user_info.token_a_key,
                &mut user_info.token_a_account,
                &user_info.token_b_key,
                &mut user_info.token_b_account,
                &user_info.pool_key,
                &mut user_info.pool_account,
                pool_token_amount,
                maximum_token_a_amount,
                maximum_token_b_amount,
            );
        }

        #[allow(clippy::too_many_arguments)]
        pub fn withdraw_all_token_types(
            &mut self,
            user_key: &Pubkey,
            pool_key: &Pubkey,
            mut pool_account: &mut Account,
            token_a_key: &Pubkey,
            mut token_a_account: &mut Account,
            token_b_key: &Pubkey,
            mut token_b_account: &mut Account,
            pool_token_amount: u64,
            minimum_token_a_amount: u64,
            minimum_token_b_amount: u64,
        ) -> ProgramResult {
            let user_transfer_authority_key = Pubkey::new_unique();
            // approve user transfer authority to take out pool tokens
            do_process_instruction(
                approve(
                    &TOKEN_PROGRAM_ID,
                    &pool_key,
                    &user_transfer_authority_key,
                    &user_key,
                    &[],
                    pool_token_amount,
                )
                    .unwrap(),
                vec![
                    &mut pool_account,
                    &mut Account::default(),
                    &mut Account::default(),
                ],
            )
                .unwrap();
            // withdraw token a and b correctly
            do_process_instruction(
                withdraw_all_token_types(
                    &SWAP_PROGRAM_ID,
                    &TOKEN_PROGRAM_ID,
                    &self.swap_key,
                    &self.authority_key,
                    &user_transfer_authority_key,
                    &self.pool_mint_key,
                    &self.pool_fee_key,
                    &pool_key,
                    &self.token_a_key,
                    &self.token_b_key,
                    &token_a_key,
                    &token_b_key,
                    WithdrawAllTokenTypes {
                        pool_token_amount,
                        minimum_token_a_amount,
                        minimum_token_b_amount,
                    },
                )
                    .unwrap(),
                vec![
                    &mut self.swap_account,
                    &mut Account::default(),
                    &mut Account::default(),
                    &mut self.pool_mint_account,
                    &mut pool_account,
                    &mut self.token_a_account,
                    &mut self.token_b_account,
                    &mut token_a_account,
                    &mut token_b_account,
                    &mut self.pool_fee_account,
                    &mut Account::default(),
                ],
            )
        }

        #[allow(clippy::too_many_arguments)]
        pub fn deposit_single_token_type_exact_amount_in(
            &mut self,
            depositor_key: &Pubkey,
            deposit_account_key: &Pubkey,
            mut deposit_token_account: &mut Account,
            deposit_pool_key: &Pubkey,
            mut deposit_pool_account: &mut Account,
            source_token_amount: u64,
            minimum_pool_token_amount: u64,
        ) -> ProgramResult {
            let user_transfer_authority_key = Pubkey::new_unique();
            do_process_instruction(
                approve(
                    &TOKEN_PROGRAM_ID,
                    &deposit_account_key,
                    &user_transfer_authority_key,
                    &depositor_key,
                    &[],
                    source_token_amount,
                )
                    .unwrap(),
                vec![
                    &mut deposit_token_account,
                    &mut Account::default(),
                    &mut Account::default(),
                ],
            )
                .unwrap();

            do_process_instruction(
                deposit_single_token_type_exact_amount_in(
                    &SWAP_PROGRAM_ID,
                    &TOKEN_PROGRAM_ID,
                    &self.swap_key,
                    &self.authority_key,
                    &user_transfer_authority_key,
                    &deposit_account_key,
                    &self.token_a_key,
                    &self.token_b_key,
                    &self.pool_mint_key,
                    &deposit_pool_key,
                    DepositSingleTokenTypeExactAmountIn {
                        source_token_amount,
                        minimum_pool_token_amount,
                    },
                )
                    .unwrap(),
                vec![
                    &mut self.swap_account,
                    &mut Account::default(),
                    &mut Account::default(),
                    &mut deposit_token_account,
                    &mut self.token_a_account,
                    &mut self.token_b_account,
                    &mut self.pool_mint_account,
                    &mut deposit_pool_account,
                    &mut Account::default(),
                ],
            )
        }

        #[allow(clippy::too_many_arguments)]
        pub fn withdraw_single_token_type_exact_amount_out(
            &mut self,
            user_key: &Pubkey,
            pool_key: &Pubkey,
            mut pool_account: &mut Account,
            destination_key: &Pubkey,
            mut destination_account: &mut Account,
            destination_token_amount: u64,
            maximum_pool_token_amount: u64,
        ) -> ProgramResult {
            let user_transfer_authority_key = Pubkey::new_unique();
            // approve user transfer authority to take out pool tokens
            do_process_instruction(
                approve(
                    &TOKEN_PROGRAM_ID,
                    &pool_key,
                    &user_transfer_authority_key,
                    &user_key,
                    &[],
                    maximum_pool_token_amount,
                )
                    .unwrap(),
                vec![
                    &mut pool_account,
                    &mut Account::default(),
                    &mut Account::default(),
                ],
            ).unwrap();
            do_process_instruction(
                withdraw_single_token_type_exact_amount_out(
                    &SWAP_PROGRAM_ID,
                    &TOKEN_PROGRAM_ID,
                    &self.swap_key,
                    &self.authority_key,
                    &user_transfer_authority_key,
                    &self.pool_mint_key,
                    &self.pool_fee_key,
                    &pool_key,
                    &self.token_a_key,
                    &self.token_b_key,
                    &destination_key,
                    WithdrawSingleTokenTypeExactAmountOut {
                        destination_token_amount,
                        maximum_pool_token_amount,
                    },
                )
                    .unwrap(),
                vec![
                    &mut self.swap_account,
                    &mut Account::default(),
                    &mut Account::default(),
                    &mut self.pool_mint_account,
                    &mut pool_account,
                    &mut self.token_a_account,
                    &mut self.token_b_account,
                    &mut destination_account,
                    &mut self.pool_fee_account,
                    &mut Account::default(),
                ],
            )
        }

        #[allow(clippy::too_many_arguments)]
        pub fn init_farming(
            &mut self,
            user_key: &Pubkey,
            clock_key: &Pubkey,
            mut clock_account: &mut Account,
            user_farming_token_key: &Pubkey,
            mut user_farming_token_account: &mut Account,
            swap_farming_token_key: &Pubkey,
            mut swap_farming_token_account: &mut Account,
            token_amount: u64,
            tokens_per_period: u64,
            period_length: u64,
        ) -> ProgramResult {
            let fee_authority = spl_token::state::Account::unpack(
                self.pool_fee_account.data.as_slice())
                .expect("")
                .owner;

            let user_transfer_authority_key = Pubkey::new_unique();
            // approve user transfer authority to take out pool tokens
            do_process_instruction(
                approve(
                    &TOKEN_PROGRAM_ID,
                    &user_farming_token_key,
                    &user_transfer_authority_key,
                    &user_key,
                    &[],
                    token_amount,
                )
                    .unwrap(),
                vec![
                    &mut user_farming_token_account,
                    &mut Account::default(),
                    &mut Account::default(),
                ],
            )
                .unwrap();

            do_process_instruction(
                initialize_farming(
                    &SWAP_PROGRAM_ID,
                    &TOKEN_PROGRAM_ID,
                    &self.swap_key,
                    &self.farming_state_key,
                    &swap_farming_token_key,
                    &user_farming_token_key,
                    &user_transfer_authority_key,
                    &self.pool_fee_key,
                    &fee_authority,
                    &self.authority_key,
                    &clock_key,
                    InitializeFarming {
                        tokens_per_period,
                        period_length,
                        token_amount,
                    },
                )
                    .unwrap(),
                vec![
                    &mut self.swap_account,
                    &mut self.farming_state_account,
                    &mut swap_farming_token_account,
                    &mut user_farming_token_account,
                    &mut Account::default(),
                    &mut self.pool_fee_account,
                    &mut Account::default(),
                    &mut Account::default(),
                    &mut clock_account,
                    &mut Account::default(),
                ],
            )
        }

        #[allow(clippy::too_many_arguments)]
        pub fn take_farming_snapshot(
            &mut self,
            clock_key: &Pubkey,
            mut clock_account: &mut Account,
        ) -> ProgramResult {
            let fee_authority = spl_token::state::Account::unpack(
                self.pool_fee_account.data.as_slice())
                .expect("")
                .owner;

            do_process_instruction(
                take_farming_snapshot(
                    &SWAP_PROGRAM_ID,
                    &self.swap_key,
                    &self.farming_state_key,
                    &self.token_freeze_key,
                    &self.pool_fee_key,
                    &fee_authority,
                    &clock_key,
                )
                    .unwrap(),
                vec![
                    &mut self.swap_account,
                    &mut self.farming_state_account,
                    &mut self.token_freeze_account,
                    &mut self.pool_fee_account,
                    &mut Account::default(),
                    &mut clock_account,
                ],
            )
        }

        #[allow(clippy::too_many_arguments)]
        pub fn start_farming(
            &mut self,
            user_key: &Pubkey,
            user_pool_token_key: &Pubkey,
            mut user_pool_token_account: &mut Account,
            farming_ticket_key: &Pubkey,
            mut farming_ticket_account: &mut Account,
            clock_key: &Pubkey,
            mut clock_account: &mut Account,
            tokens_to_freeze: u64,
        ) -> ProgramResult {
            let user_transfer_authority_key = Pubkey::new_unique();
            // approve user transfer authority to take out pool tokens
            do_process_instruction(
                approve(
                    &TOKEN_PROGRAM_ID,
                    &user_pool_token_key,
                    &user_transfer_authority_key,
                    &user_key,
                    &[],
                    tokens_to_freeze,
                )
                    .unwrap(),
                vec![
                    &mut user_pool_token_account,
                    &mut Account::default(),
                    &mut Account::default(),
                ],
            )
                .unwrap();

            do_process_instruction(
                start_farming(
                    &SWAP_PROGRAM_ID,
                    &self.swap_key,
                    &self.farming_state_key,
                    &farming_ticket_key,
                    &self.token_freeze_key,
                    &user_pool_token_key,
                    &user_transfer_authority_key,
                    &user_key,
                    &TOKEN_PROGRAM_ID,
                    &clock_key,
                    StartFarming {
                        pool_token_amount: tokens_to_freeze,
                    },
                )
                    .unwrap(),
                vec![
                    &mut self.swap_account,
                    &mut self.farming_state_account,
                    &mut farming_ticket_account,
                    &mut self.token_freeze_account,
                    &mut user_pool_token_account,
                    &mut Account::default(),
                    &mut Account::default(),
                    &mut Account::default(),
                    &mut clock_account,
                ],
            )
        }

        #[allow(clippy::too_many_arguments)]
        pub fn withdraw_farmed(
            &mut self,
            user_farming_token_key: &Pubkey,
            mut user_farming_token_account: &mut Account,
            farming_ticket_key: &Pubkey,
            mut farming_ticket_account: &mut Account,
            farming_token_key: &Pubkey,
            mut farming_token_account: &mut Account,
            user_authority_key: &Pubkey,
            clock_key: &Pubkey,
            mut clock_account: &mut Account,
        ) -> ProgramResult {
            do_process_instruction(
                withdraw_farmed(
                    &SWAP_PROGRAM_ID,
                    &TOKEN_PROGRAM_ID,
                    &self.swap_key,
                    &self.farming_state_key,
                    &farming_ticket_key,
                    &farming_token_key,
                    &self.authority_key,
                    &user_farming_token_key,
                    &user_authority_key,
                    &clock_key,
                )
                    .unwrap(),
                vec![
                    &mut self.swap_account,
                    &mut self.farming_state_account,
                    &mut farming_ticket_account,
                    &mut farming_token_account,
                    &mut Account::default(),
                    &mut user_farming_token_account,
                    &mut Account::default(),
                    &mut clock_account,
                    &mut Account::default(),
                ],
            )
        }

        #[allow(clippy::too_many_arguments)]
        pub fn end_farming(
            &mut self,
            farming_ticket_key: &Pubkey,
            mut farming_ticket_account: &mut Account,
            user_pool_token_key: &Pubkey,
            mut user_pool_token_account: &mut Account,
            user_authority_key: &Pubkey,
            clock_key: &Pubkey,
            mut clock_account: &mut Account,
        ) -> ProgramResult {
            do_process_instruction(
                end_farming(
                    &SWAP_PROGRAM_ID,
                    &TOKEN_PROGRAM_ID,
                    &self.swap_key,
                    &self.farming_state_key,
                    &farming_ticket_key,
                    &self.token_freeze_key,
                    &self.authority_key,
                    &user_pool_token_key,
                    &user_authority_key,
                    &clock_key,
                )
                    .unwrap(),
                vec![
                    &mut self.swap_account,
                    &mut self.farming_state_account,
                    &mut farming_ticket_account,
                    &mut self.token_freeze_account,
                    &mut Account::default(),
                    &mut user_pool_token_account,
                    &mut Account::default(),
                    &mut clock_account,
                    &mut Account::default(),
                ],
            )
        }
    }

    struct UserFarmingInfo {
        pub user_key: Pubkey,
        pub user_farming_token_key: Pubkey,
        pub user_farming_token_account: Account,
        pub pool_key: Pubkey,
        pub pool_account: Account,
        pub token_a_key: Pubkey,
        pub token_a_account: Account,
        pub token_b_key: Pubkey,
        pub token_b_account: Account,
        pub farming_ticket_key: Pubkey,
        pub farming_ticket_account: Account,
    }

    impl UserFarmingInfo {
        pub fn new(
            user_authority_key: Pubkey,
            farming_mint_key: Pubkey,
            mut farming_mint_account: &mut Account,
            pool_mint_key: Pubkey,
            mut pool_mint_account: &mut Account,
            token_a_mint_key: Pubkey,
            mut token_a_mint_account: &mut Account,
            token_b_mint_key: Pubkey,
            mut token_b_mint_account: &mut Account,
            token_a_amount: u64,
            token_b_amount: u64,
        ) -> Self {
            let user_key = Pubkey::new_unique();

            let (user_farming_token_key, user_farming_token_account) = mint_token(
                &TOKEN_PROGRAM_ID,
                &farming_mint_key,
                &mut farming_mint_account,
                &user_authority_key,
                &user_authority_key,
                0,
            );

            let (pool_key, pool_account) = mint_token(
                &TOKEN_PROGRAM_ID,
                &pool_mint_key,
                &mut pool_mint_account,
                &user_authority_key,
                &user_key,
                0,
            );

            let (token_a_key, token_a_account) = mint_token(
                &TOKEN_PROGRAM_ID,
                &token_a_mint_key,
                &mut token_a_mint_account,
                &user_authority_key,
                &user_key,
                token_a_amount,
            );

            let (token_b_key, token_b_account) = mint_token(
                &TOKEN_PROGRAM_ID,
                &token_b_mint_key,
                &mut token_b_mint_account,
                &user_authority_key,
                &user_key,
                token_b_amount,
            );

            let farming_ticket_key = Pubkey::new_unique();
            let farming_ticket_account = Account::new(0, FarmingTicket::LEN, &SWAP_PROGRAM_ID);

            UserFarmingInfo {
                user_key,
                user_farming_token_key,
                user_farming_token_account,
                pool_key,
                pool_account,
                token_a_key,
                token_a_account,
                token_b_key,
                token_b_account,
                farming_ticket_key,
                farming_ticket_account,
            }
        }
    }


    struct FarmingStateInfo {
        pub farming_mint_key: Pubkey,
        pub farming_mint_account: Account,
        pub swap_farming_token_key: Pubkey,
        pub swap_farming_token_account: Account,
        pub owner_farming_token_key: Pubkey,
        pub owner_farming_token_account: Account,
    }

    impl FarmingStateInfo {
        pub fn new(
            owner_token_amount: u64,
            user_authority_key: Pubkey,
            swap_authority_key: Pubkey,
        ) -> Self {
            let (farming_mint_key, mut farming_mint_account) =
                create_mint(&TOKEN_PROGRAM_ID, &user_authority_key, None);
            let (owner_farming_token_key, owner_farming_token_account) = mint_token(
                &TOKEN_PROGRAM_ID,
                &farming_mint_key,
                &mut farming_mint_account,
                &user_authority_key,
                &user_authority_key,
                owner_token_amount,
            );

            let (swap_farming_token_key, swap_farming_token_account) = mint_token(
                &TOKEN_PROGRAM_ID,
                &farming_mint_key,
                &mut farming_mint_account,
                &user_authority_key,
                &swap_authority_key,
                0,
            );

            FarmingStateInfo {
                farming_mint_key,
                farming_mint_account,
                swap_farming_token_key,
                swap_farming_token_account,
                owner_farming_token_key,
                owner_farming_token_account,
            }
        }
    }

    fn mint_minimum_balance() -> u64 {
        Rent::default().minimum_balance(spl_token::state::Mint::get_packed_len())
    }

    fn account_minimum_balance() -> u64 {
        Rent::default().minimum_balance(spl_token::state::Account::get_packed_len())
    }

    fn do_process_instruction_with_fee_constraints(
        instruction: Instruction,
        accounts: Vec<&mut Account>,
        swap_constraints: &Option<SwapConstraints>,
    ) -> ProgramResult {
        test_syscall_stubs();

        // approximate the logic in the actual runtime which runs the instruction
        // and only updates accounts if the instruction is successful
        let mut account_clones = accounts.iter().map(|x| (*x).clone()).collect::<Vec<_>>();
        let mut meta = instruction
            .accounts
            .iter()
            .zip(account_clones.iter_mut())
            .map(|(account_meta, account)| (&account_meta.pubkey, account_meta.is_signer, account))
            .collect::<Vec<_>>();
        let mut account_infos = create_is_signer_account_infos(&mut meta);
        let res = if instruction.program_id == SWAP_PROGRAM_ID {
            Processor::process_with_constraints(
                &instruction.program_id,
                &account_infos,
                &instruction.data,
                swap_constraints,
            )
        } else {
            spl_token::processor::Processor::process(
                &instruction.program_id,
                &account_infos,
                &instruction.data,
            )
        };
        if res.is_ok() {
            let mut account_metas = instruction
                .accounts
                .iter()
                .zip(accounts)
                .map(|(account_meta, account)| (&account_meta.pubkey, account))
                .collect::<Vec<_>>();

            for account_info in account_infos.iter_mut() {
                for account_meta in account_metas.iter_mut() {
                    if account_info.key == account_meta.0 {
                        let account = &mut account_meta.1;
                        account.owner = *account_info.owner;
                        account.lamports = **account_info.lamports.borrow();
                        account.data = account_info.data.borrow().to_vec();
                    }
                }
            }
        }
        res
    }

    fn do_process_instruction(
        instruction: Instruction,
        accounts: Vec<&mut Account>,
    ) -> ProgramResult {
        do_process_instruction_with_fee_constraints(instruction, accounts, &SWAP_CONSTRAINTS)
    }

    fn mint_token(
        program_id: &Pubkey,
        mint_key: &Pubkey,
        mut mint_account: &mut Account,
        mint_authority_key: &Pubkey,
        account_owner_key: &Pubkey,
        amount: u64,
    ) -> (Pubkey, Account) {
        let account_key = Pubkey::new_unique();
        let mut account_account = Account::new(
            account_minimum_balance(),
            spl_token::state::Account::get_packed_len(),
            &program_id,
        );
        let mut mint_authority_account = Account::default();
        let mut rent_sysvar_account = create_account_for_test(&Rent::free());

        do_process_instruction(
            initialize_account(&program_id, &account_key, &mint_key, account_owner_key).unwrap(),
            vec![
                &mut account_account,
                &mut mint_account,
                &mut mint_authority_account,
                &mut rent_sysvar_account,
            ],
        )
            .unwrap();

        if amount > 0 {
            do_process_instruction(
                mint_to(
                    &program_id,
                    &mint_key,
                    &account_key,
                    &mint_authority_key,
                    &[],
                    amount,
                )
                    .unwrap(),
                vec![
                    &mut mint_account,
                    &mut account_account,
                    &mut mint_authority_account,
                ],
            )
                .unwrap();
        }

        (account_key, account_account)
    }

    fn create_mint(
        program_id: &Pubkey,
        authority_key: &Pubkey,
        freeze_authority: Option<&Pubkey>,
    ) -> (Pubkey, Account) {
        let mint_key = Pubkey::new_unique();
        let mut mint_account = Account::new(
            mint_minimum_balance(),
            spl_token::state::Mint::get_packed_len(),
            &program_id,
        );
        let mut rent_sysvar_account = create_account_for_test(&Rent::free());

        do_process_instruction(
            initialize_mint(&program_id, &mint_key, authority_key, freeze_authority, 2).unwrap(),
            vec![&mut mint_account, &mut rent_sysvar_account],
        )
            .unwrap();

        (mint_key, mint_account)
    }

    fn get_clock_for_time(time: i64) -> Account {
        let mut clock = Clock::default();
        clock.unix_timestamp = time;
        Account::new_data(
            1000000000,
            &clock,
            &solana_program::system_program::ID,
        ).expect("account creation failed")
    }

    #[test]
    fn test_token_program_id_error() {
        test_syscall_stubs();
        let swap_key = Pubkey::new_unique();
        let mut mint = (Pubkey::new_unique(), Account::default());
        let mut destination = (Pubkey::new_unique(), Account::default());
        let token_program = (TOKEN_PROGRAM_ID, Account::default());
        let (authority_key, nonce) =
            Pubkey::find_program_address(&[&swap_key.to_bytes()[..]], &SWAP_PROGRAM_ID);
        let mut authority = (authority_key, Account::default());
        let swap_bytes = swap_key.to_bytes();
        let authority_signature_seeds = [&swap_bytes[..32], &[nonce]];
        let signers = &[&authority_signature_seeds[..]];
        let ix = mint_to(
            &token_program.0,
            &mint.0,
            &destination.0,
            &authority.0,
            &[],
            10,
        )
            .unwrap();
        let mint = (&mut mint).into();
        let destination = (&mut destination).into();
        let authority = (&mut authority).into();

        let err = invoke_signed(&ix, &[mint, destination, authority], signers).unwrap_err();
        assert_eq!(err, ProgramError::InvalidAccountData);
    }

    #[test]
    fn test_initialize() {
        let user_key = Pubkey::new_unique();
        let trade_fee_numerator = 25;
        let trade_fee_denominator = 10000;
        let owner_trade_fee_numerator = 5;
        let owner_trade_fee_denominator = 10000;
        let owner_withdraw_fee_numerator = 0;
        let owner_withdraw_fee_denominator = 0;
        let host_fee_numerator = 0;
        let host_fee_denominator = 0;
        let fees = Fees {
            trade_fee_numerator,
            trade_fee_denominator,
            owner_trade_fee_numerator,
            owner_trade_fee_denominator,
            owner_withdraw_fee_numerator,
            owner_withdraw_fee_denominator,
            host_fee_numerator,
            host_fee_denominator,
        };

        let token_a_amount = 1000;
        let token_b_amount = 2000;
        let pool_token_amount = 10;
        let curve_type = CurveType::ConstantProduct;
        let swap_curve = SwapCurve {
            curve_type,
            calculator: Box::new(ConstantProductCurve {}),
        };

        let mut accounts =
            SwapAccountInfo::new(&user_key, fees, swap_curve, token_a_amount, token_b_amount);

        // wrong nonce for authority_key
        {
            let old_nonce = accounts.nonce;
            accounts.nonce = old_nonce - 1;
            assert_eq!(
                Err(SwapError::InvalidProgramAddress.into()),
                accounts.initialize_swap()
            );
            accounts.nonce = old_nonce;
        }

        // uninitialized token a account
        {
            let old_account = accounts.token_a_account;
            accounts.token_a_account = Account::new(0, 0, &TOKEN_PROGRAM_ID);
            assert_eq!(
                Err(SwapError::ExpectedAccount.into()),
                accounts.initialize_swap()
            );
            accounts.token_a_account = old_account;
        }

        // uninitialized token b account
        {
            let old_account = accounts.token_b_account;
            accounts.token_b_account = Account::new(0, 0, &TOKEN_PROGRAM_ID);
            assert_eq!(
                Err(SwapError::ExpectedAccount.into()),
                accounts.initialize_swap()
            );
            accounts.token_b_account = old_account;
        }

        // uninitialized pool mint
        {
            let old_account = accounts.pool_mint_account;
            accounts.pool_mint_account = Account::new(0, 0, &TOKEN_PROGRAM_ID);
            assert_eq!(
                Err(SwapError::ExpectedMint.into()),
                accounts.initialize_swap()
            );
            accounts.pool_mint_account = old_account;
        }

        // token A account owner is not swap authority
        {
            let (_token_a_key, token_a_account) = mint_token(
                &TOKEN_PROGRAM_ID,
                &accounts.token_a_mint_key,
                &mut accounts.token_a_mint_account,
                &user_key,
                &user_key,
                0,
            );
            let old_account = accounts.token_a_account;
            accounts.token_a_account = token_a_account;
            assert_eq!(
                Err(SwapError::InvalidOwner.into()),
                accounts.initialize_swap()
            );
            accounts.token_a_account = old_account;
        }

        // token B account owner is not swap authority
        {
            let (_token_b_key, token_b_account) = mint_token(
                &TOKEN_PROGRAM_ID,
                &accounts.token_b_mint_key,
                &mut accounts.token_b_mint_account,
                &user_key,
                &user_key,
                0,
            );
            let old_account = accounts.token_b_account;
            accounts.token_b_account = token_b_account;
            assert_eq!(
                Err(SwapError::InvalidOwner.into()),
                accounts.initialize_swap()
            );
            accounts.token_b_account = old_account;
        }

        // pool token account owner is swap authority
        {
            let (_pool_token_key, pool_token_account) = mint_token(
                &TOKEN_PROGRAM_ID,
                &accounts.pool_mint_key,
                &mut accounts.pool_mint_account,
                &accounts.authority_key,
                &accounts.authority_key,
                0,
            );
            let old_account = accounts.pool_token_account;
            accounts.pool_token_account = pool_token_account;
            assert_eq!(
                Err(SwapError::InvalidOutputOwner.into()),
                accounts.initialize_swap()
            );
            accounts.pool_token_account = old_account;
        }

        // pool fee account owner is swap authority
        {
            let (_pool_fee_key, pool_fee_account) = mint_token(
                &TOKEN_PROGRAM_ID,
                &accounts.pool_mint_key,
                &mut accounts.pool_mint_account,
                &accounts.authority_key,
                &accounts.authority_key,
                0,
            );
            let old_account = accounts.pool_fee_account;
            accounts.pool_fee_account = pool_fee_account;
            assert_eq!(
                Err(SwapError::InvalidOutputOwner.into()),
                accounts.initialize_swap()
            );
            accounts.pool_fee_account = old_account;
        }

        // pool mint authority is not swap authority
        {
            let (_pool_mint_key, pool_mint_account) =
                create_mint(&TOKEN_PROGRAM_ID, &user_key, None);
            let old_mint = accounts.pool_mint_account;
            accounts.pool_mint_account = pool_mint_account;
            assert_eq!(
                Err(SwapError::InvalidOwner.into()),
                accounts.initialize_swap()
            );
            accounts.pool_mint_account = old_mint;
        }

        // pool mint token has freeze authority
        {
            let (_pool_mint_key, pool_mint_account) =
                create_mint(&TOKEN_PROGRAM_ID, &accounts.authority_key, Some(&user_key));
            let old_mint = accounts.pool_mint_account;
            accounts.pool_mint_account = pool_mint_account;
            assert_eq!(
                Err(SwapError::InvalidFreezeAuthority.into()),
                accounts.initialize_swap()
            );
            accounts.pool_mint_account = old_mint;
        }

        // token A account owned by wrong program
        {
            let (_token_a_key, mut token_a_account) = mint_token(
                &TOKEN_PROGRAM_ID,
                &accounts.token_a_mint_key,
                &mut accounts.token_a_mint_account,
                &user_key,
                &accounts.authority_key,
                token_a_amount,
            );
            token_a_account.owner = SWAP_PROGRAM_ID;
            let old_account = accounts.token_a_account;
            accounts.token_a_account = token_a_account;
            assert_eq!(
                Err(SwapError::IncorrectTokenProgramId.into()),
                accounts.initialize_swap()
            );
            accounts.token_a_account = old_account;
        }

        // token B account owned by wrong program
        {
            let (_token_b_key, mut token_b_account) = mint_token(
                &TOKEN_PROGRAM_ID,
                &accounts.token_b_mint_key,
                &mut accounts.token_b_mint_account,
                &user_key,
                &accounts.authority_key,
                token_b_amount,
            );
            token_b_account.owner = SWAP_PROGRAM_ID;
            let old_account = accounts.token_b_account;
            accounts.token_b_account = token_b_account;
            assert_eq!(
                Err(SwapError::IncorrectTokenProgramId.into()),
                accounts.initialize_swap()
            );
            accounts.token_b_account = old_account;
        }

        // empty token A account
        {
            let (_token_a_key, token_a_account) = mint_token(
                &TOKEN_PROGRAM_ID,
                &accounts.token_a_mint_key,
                &mut accounts.token_a_mint_account,
                &user_key,
                &accounts.authority_key,
                0,
            );
            let old_account = accounts.token_a_account;
            accounts.token_a_account = token_a_account;
            assert_eq!(
                Err(SwapError::EmptySupply.into()),
                accounts.initialize_swap()
            );
            accounts.token_a_account = old_account;
        }

        // empty token B account
        {
            let (_token_b_key, token_b_account) = mint_token(
                &TOKEN_PROGRAM_ID,
                &accounts.token_b_mint_key,
                &mut accounts.token_b_mint_account,
                &user_key,
                &accounts.authority_key,
                0,
            );
            let old_account = accounts.token_b_account;
            accounts.token_b_account = token_b_account;
            assert_eq!(
                Err(SwapError::EmptySupply.into()),
                accounts.initialize_swap()
            );
            accounts.token_b_account = old_account;
        }

        // invalid pool tokens
        {
            let old_mint = accounts.pool_mint_account;
            let old_pool_account = accounts.pool_token_account;

            let (_pool_mint_key, pool_mint_account) =
                create_mint(&TOKEN_PROGRAM_ID, &accounts.authority_key, None);
            accounts.pool_mint_account = pool_mint_account;

            let (_empty_pool_token_key, empty_pool_token_account) = mint_token(
                &TOKEN_PROGRAM_ID,
                &accounts.pool_mint_key,
                &mut accounts.pool_mint_account,
                &accounts.authority_key,
                &user_key,
                0,
            );

            let (_pool_token_key, pool_token_account) = mint_token(
                &TOKEN_PROGRAM_ID,
                &accounts.pool_mint_key,
                &mut accounts.pool_mint_account,
                &accounts.authority_key,
                &user_key,
                pool_token_amount,
            );

            // non-empty pool token account
            accounts.pool_token_account = pool_token_account;
            assert_eq!(
                Err(SwapError::InvalidSupply.into()),
                accounts.initialize_swap()
            );

            // pool tokens already in circulation
            accounts.pool_token_account = empty_pool_token_account;
            assert_eq!(
                Err(SwapError::InvalidSupply.into()),
                accounts.initialize_swap()
            );

            accounts.pool_mint_account = old_mint;
            accounts.pool_token_account = old_pool_account;
        }

        // pool fee account has wrong mint
        {
            let (_pool_fee_key, pool_fee_account) = mint_token(
                &TOKEN_PROGRAM_ID,
                &accounts.token_a_mint_key,
                &mut accounts.token_a_mint_account,
                &user_key,
                &user_key,
                0,
            );
            let old_account = accounts.pool_fee_account;
            accounts.pool_fee_account = pool_fee_account;
            assert_eq!(
                Err(SwapError::IncorrectPoolMint.into()),
                accounts.initialize_swap()
            );
            accounts.pool_fee_account = old_account;
        }

        // token A account is delegated
        {
            do_process_instruction(
                approve(
                    &TOKEN_PROGRAM_ID,
                    &accounts.token_a_key,
                    &user_key,
                    &accounts.authority_key,
                    &[],
                    1,
                )
                    .unwrap(),
                vec![
                    &mut accounts.token_a_account,
                    &mut Account::default(),
                    &mut Account::default(),
                ],
            )
                .unwrap();
            assert_eq!(
                Err(SwapError::InvalidDelegate.into()),
                accounts.initialize_swap()
            );

            do_process_instruction(
                revoke(
                    &TOKEN_PROGRAM_ID,
                    &accounts.token_a_key,
                    &accounts.authority_key,
                    &[],
                )
                    .unwrap(),
                vec![&mut accounts.token_a_account, &mut Account::default()],
            )
                .unwrap();
        }

        // token B account is delegated
        {
            do_process_instruction(
                approve(
                    &TOKEN_PROGRAM_ID,
                    &accounts.token_b_key,
                    &user_key,
                    &accounts.authority_key,
                    &[],
                    1,
                )
                    .unwrap(),
                vec![
                    &mut accounts.token_b_account,
                    &mut Account::default(),
                    &mut Account::default(),
                ],
            )
                .unwrap();
            assert_eq!(
                Err(SwapError::InvalidDelegate.into()),
                accounts.initialize_swap()
            );

            do_process_instruction(
                revoke(
                    &TOKEN_PROGRAM_ID,
                    &accounts.token_b_key,
                    &accounts.authority_key,
                    &[],
                )
                    .unwrap(),
                vec![&mut accounts.token_b_account, &mut Account::default()],
            )
                .unwrap();
        }

        // token A account has close authority
        {
            do_process_instruction(
                set_authority(
                    &TOKEN_PROGRAM_ID,
                    &accounts.token_a_key,
                    Some(&user_key),
                    AuthorityType::CloseAccount,
                    &accounts.authority_key,
                    &[],
                )
                    .unwrap(),
                vec![&mut accounts.token_a_account, &mut Account::default()],
            )
                .unwrap();
            assert_eq!(
                Err(SwapError::InvalidCloseAuthority.into()),
                accounts.initialize_swap()
            );

            do_process_instruction(
                set_authority(
                    &TOKEN_PROGRAM_ID,
                    &accounts.token_a_key,
                    None,
                    AuthorityType::CloseAccount,
                    &user_key,
                    &[],
                )
                    .unwrap(),
                vec![&mut accounts.token_a_account, &mut Account::default()],
            )
                .unwrap();
        }

        // token B account has close authority
        {
            do_process_instruction(
                set_authority(
                    &TOKEN_PROGRAM_ID,
                    &accounts.token_b_key,
                    Some(&user_key),
                    AuthorityType::CloseAccount,
                    &accounts.authority_key,
                    &[],
                )
                    .unwrap(),
                vec![&mut accounts.token_b_account, &mut Account::default()],
            )
                .unwrap();
            assert_eq!(
                Err(SwapError::InvalidCloseAuthority.into()),
                accounts.initialize_swap()
            );

            do_process_instruction(
                set_authority(
                    &TOKEN_PROGRAM_ID,
                    &accounts.token_b_key,
                    None,
                    AuthorityType::CloseAccount,
                    &user_key,
                    &[],
                )
                    .unwrap(),
                vec![&mut accounts.token_b_account, &mut Account::default()],
            )
                .unwrap();
        }

        // wrong token program id
        {
            let wrong_program_id = Pubkey::new_unique();
            assert_eq!(
                Err(SwapError::IncorrectTokenProgramId.into()),
                do_process_instruction(
                    initialize(
                        &SWAP_PROGRAM_ID,
                        &wrong_program_id,
                        &accounts.swap_key,
                        &accounts.authority_key,
                        &accounts.token_a_key,
                        &accounts.token_b_key,
                        &accounts.pool_mint_key,
                        &accounts.pool_fee_key,
                        &accounts.pool_token_key,
                        accounts.nonce,
                        accounts.fees.clone(),
                        accounts.swap_curve.clone(),
                        &accounts.farming_state_key,
                        &accounts.token_freeze_key,
                    )
                        .unwrap(),
                    vec![
                        &mut accounts.swap_account,
                        &mut Account::default(),
                        &mut accounts.token_a_account,
                        &mut accounts.token_b_account,
                        &mut accounts.pool_mint_account,
                        &mut accounts.pool_fee_account,
                        &mut accounts.pool_token_account,
                        &mut Account::default(),
                        &mut accounts.farming_state_account,
                        &mut accounts.token_freeze_account,
                    ],
                )
            );
        }

        // create swap with same token A and B
        {
            let (_token_a_repeat_key, token_a_repeat_account) = mint_token(
                &TOKEN_PROGRAM_ID,
                &accounts.token_a_mint_key,
                &mut accounts.token_a_mint_account,
                &user_key,
                &accounts.authority_key,
                10,
            );
            let old_account = accounts.token_b_account;
            accounts.token_b_account = token_a_repeat_account;
            assert_eq!(
                Err(SwapError::RepeatedMint.into()),
                accounts.initialize_swap()
            );
            accounts.token_b_account = old_account;
        }

        // create valid swap
        accounts.initialize_swap().unwrap();

        // wrong owner key in constraint
        {
            let new_key = Pubkey::new_unique();
            let trade_fee_numerator = 25;
            let trade_fee_denominator = 10000;
            let owner_trade_fee_numerator = 5;
            let owner_trade_fee_denominator = 10000;
            let host_fee_numerator = 0;
            let host_fee_denominator = 0;
            let fees = Fees {
                trade_fee_numerator,
                trade_fee_denominator,
                owner_trade_fee_numerator,
                owner_trade_fee_denominator,
                owner_withdraw_fee_numerator,
                owner_withdraw_fee_denominator,
                host_fee_numerator,
                host_fee_denominator,
            };
            let curve = ConstantProductCurve {};
            let swap_curve = SwapCurve {
                curve_type: CurveType::ConstantProduct,
                calculator: Box::new(curve),
            };
            let owner_key = &new_key.to_string();
            let valid_curve_types = &[CurveType::ConstantProduct];
            let constraints = Some(SwapConstraints {
                owner_key,
                valid_curve_types,
                fees: &fees,
            });
            let mut accounts = SwapAccountInfo::new(
                &user_key,
                fees.clone(),
                swap_curve,
                token_a_amount,
                token_b_amount,
            );
            assert_eq!(
                Err(SwapError::InvalidOwner.into()),
                do_process_instruction_with_fee_constraints(
                    initialize(
                        &SWAP_PROGRAM_ID,
                        &TOKEN_PROGRAM_ID,
                        &accounts.swap_key,
                        &accounts.authority_key,
                        &accounts.token_a_key,
                        &accounts.token_b_key,
                        &accounts.pool_mint_key,
                        &accounts.pool_fee_key,
                        &accounts.pool_token_key,
                        accounts.nonce,
                        accounts.fees.clone(),
                        accounts.swap_curve.clone(),
                        &accounts.farming_state_key,
                        &accounts.token_freeze_key,
                    )
                        .unwrap(),
                    vec![
                        &mut accounts.swap_account,
                        &mut Account::default(),
                        &mut accounts.token_a_account,
                        &mut accounts.token_b_account,
                        &mut accounts.pool_mint_account,
                        &mut accounts.pool_fee_account,
                        &mut accounts.pool_token_account,
                        &mut Account::default(),
                        &mut accounts.farming_state_account,
                        &mut accounts.token_freeze_account,
                    ],
                    &constraints,
                )
            );
        }

        // wrong fee in constraint
        {
            let trade_fee_numerator = 25;
            let trade_fee_denominator = 10000;
            let owner_trade_fee_numerator = 5;
            let owner_trade_fee_denominator = 10000;
            let host_fee_numerator = 0;
            let host_fee_denominator = 0;
            let fees = Fees {
                trade_fee_numerator,
                trade_fee_denominator,
                owner_trade_fee_numerator,
                owner_trade_fee_denominator,
                owner_withdraw_fee_numerator,
                owner_withdraw_fee_denominator,
                host_fee_numerator,
                host_fee_denominator,
            };
            let curve = ConstantProductCurve {};
            let swap_curve = SwapCurve {
                curve_type: CurveType::ConstantProduct,
                calculator: Box::new(curve),
            };
            let mut owner_key = user_key.to_string();
            if let Some(constraints) = crate::constraints::SWAP_CONSTRAINTS {
                owner_key = constraints.owner_key.to_string();
            }
            let owner_key = &owner_key;
            let valid_curve_types = &[CurveType::ConstantProduct];
            let constraints = Some(SwapConstraints {
                owner_key,
                valid_curve_types,
                fees: &fees,
            });
            let mut bad_fees = fees.clone();
            bad_fees.trade_fee_numerator = trade_fee_numerator - 1;
            let mut accounts = SwapAccountInfo::new(
                &user_key,
                bad_fees,
                swap_curve,
                token_a_amount,
                token_b_amount,
            );
            assert_eq!(
                Err(SwapError::InvalidFee.into()),
                do_process_instruction_with_fee_constraints(
                    initialize(
                        &SWAP_PROGRAM_ID,
                        &TOKEN_PROGRAM_ID,
                        &accounts.swap_key,
                        &accounts.authority_key,
                        &accounts.token_a_key,
                        &accounts.token_b_key,
                        &accounts.pool_mint_key,
                        &accounts.pool_fee_key,
                        &accounts.pool_token_key,
                        accounts.nonce,
                        accounts.fees.clone(),
                        accounts.swap_curve.clone(),
                        &accounts.farming_state_key,
                        &accounts.token_freeze_key,
                    )
                        .unwrap(),
                    vec![
                        &mut accounts.swap_account,
                        &mut Account::default(),
                        &mut accounts.token_a_account,
                        &mut accounts.token_b_account,
                        &mut accounts.pool_mint_account,
                        &mut accounts.pool_fee_account,
                        &mut accounts.pool_token_account,
                        &mut Account::default(),
                        &mut accounts.farming_state_account,
                        &mut accounts.token_freeze_account,
                    ],
                    &constraints,
                )
            );
        }

        // create valid swap with constraints
        {
            let trade_fee_numerator = 25;
            let trade_fee_denominator = 10000;
            let owner_trade_fee_numerator = 5;
            let owner_trade_fee_denominator = 10000;
            let host_fee_numerator = 0;
            let host_fee_denominator = 0;
            let fees = Fees {
                trade_fee_numerator,
                trade_fee_denominator,
                owner_trade_fee_numerator,
                owner_trade_fee_denominator,
                owner_withdraw_fee_numerator,
                owner_withdraw_fee_denominator,
                host_fee_numerator,
                host_fee_denominator,
            };
            let curve = ConstantProductCurve {};
            let swap_curve = SwapCurve {
                curve_type: CurveType::ConstantProduct,
                calculator: Box::new(curve),
            };
            let mut owner_key = user_key.to_string();
            if let Some(constraints) = crate::constraints::SWAP_CONSTRAINTS {
                owner_key = constraints.owner_key.to_string();
            }
            let owner_key = &owner_key;
            let valid_curve_types = &[CurveType::ConstantProduct];
            let constraints = Some(SwapConstraints {
                owner_key,
                valid_curve_types,
                fees: &fees,
            });
            let mut accounts = SwapAccountInfo::new(
                &user_key,
                fees.clone(),
                swap_curve,
                token_a_amount,
                token_b_amount,
            );
            do_process_instruction_with_fee_constraints(
                initialize(
                    &SWAP_PROGRAM_ID,
                    &TOKEN_PROGRAM_ID,
                    &accounts.swap_key,
                    &accounts.authority_key,
                    &accounts.token_a_key,
                    &accounts.token_b_key,
                    &accounts.pool_mint_key,
                    &accounts.pool_fee_key,
                    &accounts.pool_token_key,
                    accounts.nonce,
                    accounts.fees,
                    accounts.swap_curve.clone(),
                    &accounts.farming_state_key,
                    &accounts.token_freeze_key,
                )
                    .unwrap(),
                vec![
                    &mut accounts.swap_account,
                    &mut Account::default(),
                    &mut accounts.token_a_account,
                    &mut accounts.token_b_account,
                    &mut accounts.pool_mint_account,
                    &mut accounts.pool_fee_account,
                    &mut accounts.pool_token_account,
                    &mut Account::default(),
                    &mut accounts.farming_state_account,
                    &mut accounts.token_freeze_account,
                ],
                &constraints,
            )
                .unwrap();
        }

        // create again
        {
            assert_eq!(
                Err(SwapError::AlreadyInUse.into()),
                accounts.initialize_swap()
            );
        }
        let swap_state = SwapVersion::unpack(&accounts.swap_account.data).unwrap();
        assert_eq!(swap_state.is_initialized(), true);
        assert_eq!(swap_state.nonce(), accounts.nonce);
        assert_eq!(
            swap_state.swap_curve().curve_type,
            accounts.swap_curve.curve_type
        );
        assert_eq!(*swap_state.token_a_account(), accounts.token_a_key);
        assert_eq!(*swap_state.token_b_account(), accounts.token_b_key);
        assert_eq!(*swap_state.pool_mint(), accounts.pool_mint_key);
        assert_eq!(*swap_state.token_a_mint(), accounts.token_a_mint_key);
        assert_eq!(*swap_state.token_b_mint(), accounts.token_b_mint_key);
        assert_eq!(*swap_state.pool_fee_account(), accounts.pool_fee_key);
        let token_a = spl_token::state::Account::unpack(&accounts.token_a_account.data).unwrap();
        assert_eq!(token_a.amount, token_a_amount);
        let token_b = spl_token::state::Account::unpack(&accounts.token_b_account.data).unwrap();
        assert_eq!(token_b.amount, token_b_amount);
        let pool_account =
            spl_token::state::Account::unpack(&accounts.pool_token_account.data).unwrap();
        let pool_mint = spl_token::state::Mint::unpack(&accounts.pool_mint_account.data).unwrap();
        assert_eq!(pool_mint.supply, pool_account.amount);
    }

    #[test]
    fn test_deposit() {
        let user_key = Pubkey::new_unique();
        let depositor_key = Pubkey::new_unique();
        let trade_fee_numerator = 25;
        let trade_fee_denominator = 10000;
        let owner_trade_fee_numerator = 5;
        let owner_trade_fee_denominator = 10000;
        let owner_withdraw_fee_numerator = 0;
        let owner_withdraw_fee_denominator = 0;
        let host_fee_numerator = 0;
        let host_fee_denominator = 0;

        let fees = Fees {
            trade_fee_numerator,
            trade_fee_denominator,
            owner_trade_fee_numerator,
            owner_trade_fee_denominator,
            owner_withdraw_fee_numerator,
            owner_withdraw_fee_denominator,
            host_fee_numerator,
            host_fee_denominator,
        };

        let token_a_amount = 1000;
        let token_b_amount = 9000;
        let curve_type = CurveType::ConstantProduct;
        let swap_curve = SwapCurve {
            curve_type,
            calculator: Box::new(ConstantProductCurve {}),
        };

        let mut accounts =
            SwapAccountInfo::new(&user_key, fees, swap_curve, token_a_amount, token_b_amount);

        // depositing 10% of the current pool amount in token A and B means
        // that our pool tokens will be worth 1 / 10 of the current pool amount
        let pool_amount = INITIAL_SWAP_POOL_AMOUNT / 10;
        let deposit_a = token_a_amount / 10;
        let deposit_b = token_b_amount / 10;

        // swap not initialized
        {
            let (
                token_a_key,
                mut token_a_account,
                token_b_key,
                mut token_b_account,
                pool_key,
                mut pool_account,
            ) = accounts.setup_token_accounts(&user_key, &depositor_key, deposit_a, deposit_b, 0);
            assert_eq!(
                Err(ProgramError::UninitializedAccount),
                accounts.deposit_all_token_types(
                    &depositor_key,
                    &token_a_key,
                    &mut token_a_account,
                    &token_b_key,
                    &mut token_b_account,
                    &pool_key,
                    &mut pool_account,
                    pool_amount.try_into().unwrap(),
                    deposit_a,
                    deposit_b,
                )
            );
        }

        accounts.initialize_swap().unwrap();

        // wrong owner for swap account
        {
            let (
                token_a_key,
                mut token_a_account,
                token_b_key,
                mut token_b_account,
                pool_key,
                mut pool_account,
            ) = accounts.setup_token_accounts(&user_key, &depositor_key, deposit_a, deposit_b, 0);
            let old_swap_account = accounts.swap_account;
            let mut wrong_swap_account = old_swap_account.clone();
            wrong_swap_account.owner = TOKEN_PROGRAM_ID;
            accounts.swap_account = wrong_swap_account;
            assert_eq!(
                Err(ProgramError::IncorrectProgramId),
                accounts.deposit_all_token_types(
                    &depositor_key,
                    &token_a_key,
                    &mut token_a_account,
                    &token_b_key,
                    &mut token_b_account,
                    &pool_key,
                    &mut pool_account,
                    pool_amount.try_into().unwrap(),
                    deposit_a,
                    deposit_b,
                )
            );
            accounts.swap_account = old_swap_account;
        }

        // wrong nonce for authority_key
        {
            let (
                token_a_key,
                mut token_a_account,
                token_b_key,
                mut token_b_account,
                pool_key,
                mut pool_account,
            ) = accounts.setup_token_accounts(&user_key, &depositor_key, deposit_a, deposit_b, 0);
            let old_authority = accounts.authority_key;
            let (bad_authority_key, _nonce) = Pubkey::find_program_address(
                &[&accounts.swap_key.to_bytes()[..]],
                &TOKEN_PROGRAM_ID,
            );
            accounts.authority_key = bad_authority_key;
            assert_eq!(
                Err(SwapError::InvalidProgramAddress.into()),
                accounts.deposit_all_token_types(
                    &depositor_key,
                    &token_a_key,
                    &mut token_a_account,
                    &token_b_key,
                    &mut token_b_account,
                    &pool_key,
                    &mut pool_account,
                    pool_amount.try_into().unwrap(),
                    deposit_a,
                    deposit_b,
                )
            );
            accounts.authority_key = old_authority;
        }

        // not enough token A
        {
            let (
                token_a_key,
                mut token_a_account,
                token_b_key,
                mut token_b_account,
                pool_key,
                mut pool_account,
            ) = accounts.setup_token_accounts(
                &user_key,
                &depositor_key,
                deposit_a / 2,
                deposit_b,
                0,
            );
            assert_eq!(
                Err(TokenError::InsufficientFunds.into()),
                accounts.deposit_all_token_types(
                    &depositor_key,
                    &token_a_key,
                    &mut token_a_account,
                    &token_b_key,
                    &mut token_b_account,
                    &pool_key,
                    &mut pool_account,
                    pool_amount.try_into().unwrap(),
                    deposit_a,
                    deposit_b,
                )
            );
        }

        // not enough token B
        {
            let (
                token_a_key,
                mut token_a_account,
                token_b_key,
                mut token_b_account,
                pool_key,
                mut pool_account,
            ) = accounts.setup_token_accounts(
                &user_key,
                &depositor_key,
                deposit_a,
                deposit_b / 2,
                0,
            );
            assert_eq!(
                Err(TokenError::InsufficientFunds.into()),
                accounts.deposit_all_token_types(
                    &depositor_key,
                    &token_a_key,
                    &mut token_a_account,
                    &token_b_key,
                    &mut token_b_account,
                    &pool_key,
                    &mut pool_account,
                    pool_amount.try_into().unwrap(),
                    deposit_a,
                    deposit_b,
                )
            );
        }

        // wrong swap token accounts
        {
            let (
                token_a_key,
                mut token_a_account,
                token_b_key,
                mut token_b_account,
                pool_key,
                mut pool_account,
            ) = accounts.setup_token_accounts(&user_key, &depositor_key, deposit_a, deposit_b, 0);
            assert_eq!(
                Err(TokenError::MintMismatch.into()),
                accounts.deposit_all_token_types(
                    &depositor_key,
                    &token_b_key,
                    &mut token_b_account,
                    &token_a_key,
                    &mut token_a_account,
                    &pool_key,
                    &mut pool_account,
                    pool_amount.try_into().unwrap(),
                    deposit_a,
                    deposit_b,
                )
            );
        }

        // wrong pool token account
        {
            let (
                token_a_key,
                mut token_a_account,
                token_b_key,
                mut token_b_account,
                _pool_key,
                mut _pool_account,
            ) = accounts.setup_token_accounts(&user_key, &depositor_key, deposit_a, deposit_b, 0);
            let (
                wrong_token_key,
                mut wrong_token_account,
                _token_b_key,
                mut _token_b_account,
                _pool_key,
                mut _pool_account,
            ) = accounts.setup_token_accounts(&user_key, &depositor_key, deposit_a, deposit_b, 0);
            assert_eq!(
                Err(TokenError::MintMismatch.into()),
                accounts.deposit_all_token_types(
                    &depositor_key,
                    &token_a_key,
                    &mut token_a_account,
                    &token_b_key,
                    &mut token_b_account,
                    &wrong_token_key,
                    &mut wrong_token_account,
                    pool_amount.try_into().unwrap(),
                    deposit_a,
                    deposit_b,
                )
            );
        }

        // no approval
        {
            let (
                token_a_key,
                mut token_a_account,
                token_b_key,
                mut token_b_account,
                pool_key,
                mut pool_account,
            ) = accounts.setup_token_accounts(&user_key, &depositor_key, deposit_a, deposit_b, 0);
            let user_transfer_authority_key = Pubkey::new_unique();
            assert_eq!(
                Err(TokenError::OwnerMismatch.into()),
                do_process_instruction(
                    deposit_all_token_types(
                        &SWAP_PROGRAM_ID,
                        &TOKEN_PROGRAM_ID,
                        &accounts.swap_key,
                        &accounts.authority_key,
                        &user_transfer_authority_key,
                        &token_a_key,
                        &token_b_key,
                        &accounts.token_a_key,
                        &accounts.token_b_key,
                        &accounts.pool_mint_key,
                        &pool_key,
                        DepositAllTokenTypes {
                            pool_token_amount: pool_amount.try_into().unwrap(),
                            maximum_token_a_amount: deposit_a,
                            maximum_token_b_amount: deposit_b,
                        },
                    )
                        .unwrap(),
                    vec![
                        &mut accounts.swap_account,
                        &mut Account::default(),
                        &mut Account::default(),
                        &mut token_a_account,
                        &mut token_b_account,
                        &mut accounts.token_a_account,
                        &mut accounts.token_b_account,
                        &mut accounts.pool_mint_account,
                        &mut pool_account,
                        &mut Account::default(),
                    ],
                )
            );
        }

        // wrong token program id
        {
            let (
                token_a_key,
                mut token_a_account,
                token_b_key,
                mut token_b_account,
                pool_key,
                mut pool_account,
            ) = accounts.setup_token_accounts(&user_key, &depositor_key, deposit_a, deposit_b, 0);
            let wrong_key = Pubkey::new_unique();
            assert_eq!(
                Err(SwapError::IncorrectTokenProgramId.into()),
                do_process_instruction(
                    deposit_all_token_types(
                        &SWAP_PROGRAM_ID,
                        &wrong_key,
                        &accounts.swap_key,
                        &accounts.authority_key,
                        &accounts.authority_key,
                        &token_a_key,
                        &token_b_key,
                        &accounts.token_a_key,
                        &accounts.token_b_key,
                        &accounts.pool_mint_key,
                        &pool_key,
                        DepositAllTokenTypes {
                            pool_token_amount: pool_amount.try_into().unwrap(),
                            maximum_token_a_amount: deposit_a,
                            maximum_token_b_amount: deposit_b,
                        },
                    )
                        .unwrap(),
                    vec![
                        &mut accounts.swap_account,
                        &mut Account::default(),
                        &mut Account::default(),
                        &mut token_a_account,
                        &mut token_b_account,
                        &mut accounts.token_a_account,
                        &mut accounts.token_b_account,
                        &mut accounts.pool_mint_account,
                        &mut pool_account,
                        &mut Account::default(),
                    ],
                )
            );
        }

        // wrong swap token accounts
        {
            let (
                token_a_key,
                mut token_a_account,
                token_b_key,
                mut token_b_account,
                pool_key,
                mut pool_account,
            ) = accounts.setup_token_accounts(&user_key, &depositor_key, deposit_a, deposit_b, 0);

            let old_a_key = accounts.token_a_key;
            let old_a_account = accounts.token_a_account;

            accounts.token_a_key = token_a_key;
            accounts.token_a_account = token_a_account.clone();

            // wrong swap token a account
            assert_eq!(
                Err(SwapError::IncorrectSwapAccount.into()),
                accounts.deposit_all_token_types(
                    &depositor_key,
                    &token_a_key,
                    &mut token_a_account,
                    &token_b_key,
                    &mut token_b_account,
                    &pool_key,
                    &mut pool_account,
                    pool_amount.try_into().unwrap(),
                    deposit_a,
                    deposit_b,
                )
            );

            accounts.token_a_key = old_a_key;
            accounts.token_a_account = old_a_account;

            let old_b_key = accounts.token_b_key;
            let old_b_account = accounts.token_b_account;

            accounts.token_b_key = token_b_key;
            accounts.token_b_account = token_b_account.clone();

            // wrong swap token b account
            assert_eq!(
                Err(SwapError::IncorrectSwapAccount.into()),
                accounts.deposit_all_token_types(
                    &depositor_key,
                    &token_a_key,
                    &mut token_a_account,
                    &token_b_key,
                    &mut token_b_account,
                    &pool_key,
                    &mut pool_account,
                    pool_amount.try_into().unwrap(),
                    deposit_a,
                    deposit_b,
                )
            );

            accounts.token_b_key = old_b_key;
            accounts.token_b_account = old_b_account;
        }

        // wrong mint
        {
            let (
                token_a_key,
                mut token_a_account,
                token_b_key,
                mut token_b_account,
                pool_key,
                mut pool_account,
            ) = accounts.setup_token_accounts(&user_key, &depositor_key, deposit_a, deposit_b, 0);
            let (pool_mint_key, pool_mint_account) =
                create_mint(&TOKEN_PROGRAM_ID, &accounts.authority_key, None);
            let old_pool_key = accounts.pool_mint_key;
            let old_pool_account = accounts.pool_mint_account;
            accounts.pool_mint_key = pool_mint_key;
            accounts.pool_mint_account = pool_mint_account;

            assert_eq!(
                Err(SwapError::IncorrectPoolMint.into()),
                accounts.deposit_all_token_types(
                    &depositor_key,
                    &token_a_key,
                    &mut token_a_account,
                    &token_b_key,
                    &mut token_b_account,
                    &pool_key,
                    &mut pool_account,
                    pool_amount.try_into().unwrap(),
                    deposit_a,
                    deposit_b,
                )
            );

            accounts.pool_mint_key = old_pool_key;
            accounts.pool_mint_account = old_pool_account;
        }

        // deposit 1 pool token fails beacuse it equates to 0 swap tokens
        {
            let (
                token_a_key,
                mut token_a_account,
                token_b_key,
                mut token_b_account,
                pool_key,
                mut pool_account,
            ) = accounts.setup_token_accounts(&user_key, &depositor_key, deposit_a, deposit_b, 0);
            assert_eq!(
                Err(SwapError::ZeroTradingTokens.into()),
                accounts.deposit_all_token_types(
                    &depositor_key,
                    &token_a_key,
                    &mut token_a_account,
                    &token_b_key,
                    &mut token_b_account,
                    &pool_key,
                    &mut pool_account,
                    1,
                    deposit_a,
                    deposit_b,
                )
            );
        }

        // slippage exceeded
        {
            let (
                token_a_key,
                mut token_a_account,
                token_b_key,
                mut token_b_account,
                pool_key,
                mut pool_account,
            ) = accounts.setup_token_accounts(&user_key, &depositor_key, deposit_a, deposit_b, 0);
            // maximum A amount in too low
            assert_eq!(
                Err(SwapError::ExceededSlippage.into()),
                accounts.deposit_all_token_types(
                    &depositor_key,
                    &token_a_key,
                    &mut token_a_account,
                    &token_b_key,
                    &mut token_b_account,
                    &pool_key,
                    &mut pool_account,
                    pool_amount.try_into().unwrap(),
                    deposit_a / 10,
                    deposit_b,
                )
            );
            // maximum B amount in too low
            assert_eq!(
                Err(SwapError::ExceededSlippage.into()),
                accounts.deposit_all_token_types(
                    &depositor_key,
                    &token_a_key,
                    &mut token_a_account,
                    &token_b_key,
                    &mut token_b_account,
                    &pool_key,
                    &mut pool_account,
                    pool_amount.try_into().unwrap(),
                    deposit_a,
                    deposit_b / 10,
                )
            );
        }

        // invalid input: can't use swap pool tokens as source
        {
            let (
                _token_a_key,
                _token_a_account,
                _token_b_key,
                _token_b_account,
                pool_key,
                mut pool_account,
            ) = accounts.setup_token_accounts(&user_key, &depositor_key, deposit_a, deposit_b, 0);
            let swap_token_a_key = accounts.token_a_key;
            let mut swap_token_a_account = accounts.get_token_account(&swap_token_a_key).clone();
            let swap_token_b_key = accounts.token_b_key;
            let mut swap_token_b_account = accounts.get_token_account(&swap_token_b_key).clone();
            let authority_key = accounts.authority_key;
            assert_eq!(
                Err(SwapError::InvalidInput.into()),
                accounts.deposit_all_token_types(
                    &authority_key,
                    &swap_token_a_key,
                    &mut swap_token_a_account,
                    &swap_token_b_key,
                    &mut swap_token_b_account,
                    &pool_key,
                    &mut pool_account,
                    pool_amount.try_into().unwrap(),
                    deposit_a,
                    deposit_b,
                )
            );
        }

        // correctly deposit
        {
            let (
                token_a_key,
                mut token_a_account,
                token_b_key,
                mut token_b_account,
                pool_key,
                mut pool_account,
            ) = accounts.setup_token_accounts(&user_key, &depositor_key, deposit_a, deposit_b, 0);
            accounts
                .deposit_all_token_types(
                    &depositor_key,
                    &token_a_key,
                    &mut token_a_account,
                    &token_b_key,
                    &mut token_b_account,
                    &pool_key,
                    &mut pool_account,
                    pool_amount.try_into().unwrap(),
                    deposit_a,
                    deposit_b,
                )
                .unwrap();

            let swap_token_a =
                spl_token::state::Account::unpack(&accounts.token_a_account.data).unwrap();
            assert_eq!(swap_token_a.amount, deposit_a + token_a_amount);
            let swap_token_b =
                spl_token::state::Account::unpack(&accounts.token_b_account.data).unwrap();
            assert_eq!(swap_token_b.amount, deposit_b + token_b_amount);
            let token_a = spl_token::state::Account::unpack(&token_a_account.data).unwrap();
            assert_eq!(token_a.amount, 0);
            let token_b = spl_token::state::Account::unpack(&token_b_account.data).unwrap();
            assert_eq!(token_b.amount, 0);
            let pool_account = spl_token::state::Account::unpack(&pool_account.data).unwrap();
            let swap_pool_account =
                spl_token::state::Account::unpack(&accounts.pool_token_account.data).unwrap();
            let pool_mint =
                spl_token::state::Mint::unpack(&accounts.pool_mint_account.data).unwrap();
            assert_eq!(
                pool_mint.supply,
                pool_account.amount + swap_pool_account.amount
            );
        }
    }

    #[test]
    fn test_withdraw() {
        let user_key = Pubkey::new_unique();
        let trade_fee_numerator = 25;
        let trade_fee_denominator = 10000;
        let owner_trade_fee_numerator = 5;
        let owner_trade_fee_denominator = 10000;
        let owner_withdraw_fee_numerator = 0;
        let owner_withdraw_fee_denominator = 0;
        let host_fee_numerator = 0;
        let host_fee_denominator = 0;

        let fees = Fees {
            trade_fee_numerator,
            trade_fee_denominator,
            owner_trade_fee_numerator,
            owner_trade_fee_denominator,
            owner_withdraw_fee_numerator,
            owner_withdraw_fee_denominator,
            host_fee_numerator,
            host_fee_denominator,
        };

        let token_a_amount = 1000000;
        let token_b_amount = 2000000;
        let curve_type = CurveType::ConstantProduct;
        let swap_curve = SwapCurve {
            curve_type,
            calculator: Box::new(ConstantProductCurve {}),
        };

        let withdrawer_key = Pubkey::new_unique();
        let initial_a = token_a_amount / 10;
        let initial_b = token_b_amount / 10;
        let initial_pool = swap_curve.calculator.new_pool_supply() / 10;
        let withdraw_amount = initial_pool / 4;
        let minimum_token_a_amount = initial_a / 40;
        let minimum_token_b_amount = initial_b / 40;

        let mut accounts =
            SwapAccountInfo::new(&user_key, fees, swap_curve, token_a_amount, token_b_amount);

        // swap not initialized
        {
            let (
                token_a_key,
                mut token_a_account,
                token_b_key,
                mut token_b_account,
                pool_key,
                mut pool_account,
            ) = accounts.setup_token_accounts(&user_key, &withdrawer_key, initial_a, initial_b, 0);
            assert_eq!(
                Err(ProgramError::UninitializedAccount),
                accounts.withdraw_all_token_types(
                    &withdrawer_key,
                    &pool_key,
                    &mut pool_account,
                    &token_a_key,
                    &mut token_a_account,
                    &token_b_key,
                    &mut token_b_account,
                    withdraw_amount.try_into().unwrap(),
                    minimum_token_a_amount,
                    minimum_token_b_amount,
                )
            );
        }

        accounts.initialize_swap().unwrap();

        // wrong owner for swap account
        {
            let (
                token_a_key,
                mut token_a_account,
                token_b_key,
                mut token_b_account,
                pool_key,
                mut pool_account,
            ) = accounts.setup_token_accounts(&user_key, &withdrawer_key, initial_a, initial_b, 0);
            let old_swap_account = accounts.swap_account;
            let mut wrong_swap_account = old_swap_account.clone();
            wrong_swap_account.owner = TOKEN_PROGRAM_ID;
            accounts.swap_account = wrong_swap_account;
            assert_eq!(
                Err(ProgramError::IncorrectProgramId),
                accounts.withdraw_all_token_types(
                    &withdrawer_key,
                    &pool_key,
                    &mut pool_account,
                    &token_a_key,
                    &mut token_a_account,
                    &token_b_key,
                    &mut token_b_account,
                    withdraw_amount.try_into().unwrap(),
                    minimum_token_a_amount,
                    minimum_token_b_amount,
                )
            );
            accounts.swap_account = old_swap_account;
        }

        // wrong nonce for authority_key
        {
            let (
                token_a_key,
                mut token_a_account,
                token_b_key,
                mut token_b_account,
                pool_key,
                mut pool_account,
            ) = accounts.setup_token_accounts(&user_key, &withdrawer_key, initial_a, initial_b, 0);
            let old_authority = accounts.authority_key;
            let (bad_authority_key, _nonce) = Pubkey::find_program_address(
                &[&accounts.swap_key.to_bytes()[..]],
                &TOKEN_PROGRAM_ID,
            );
            accounts.authority_key = bad_authority_key;
            assert_eq!(
                Err(SwapError::InvalidProgramAddress.into()),
                accounts.withdraw_all_token_types(
                    &withdrawer_key,
                    &pool_key,
                    &mut pool_account,
                    &token_a_key,
                    &mut token_a_account,
                    &token_b_key,
                    &mut token_b_account,
                    withdraw_amount.try_into().unwrap(),
                    minimum_token_a_amount,
                    minimum_token_b_amount,
                )
            );
            accounts.authority_key = old_authority;
        }

        // not enough pool tokens
        {
            let (
                token_a_key,
                mut token_a_account,
                token_b_key,
                mut token_b_account,
                pool_key,
                mut pool_account,
            ) = accounts.setup_token_accounts(
                &user_key,
                &withdrawer_key,
                initial_a,
                initial_b,
                to_u64(withdraw_amount).unwrap() / 2u64,
            );
            assert_eq!(
                Err(TokenError::InsufficientFunds.into()),
                accounts.withdraw_all_token_types(
                    &withdrawer_key,
                    &pool_key,
                    &mut pool_account,
                    &token_a_key,
                    &mut token_a_account,
                    &token_b_key,
                    &mut token_b_account,
                    withdraw_amount.try_into().unwrap(),
                    minimum_token_a_amount / 2,
                    minimum_token_b_amount / 2,
                )
            );
        }

        // wrong token a / b accounts
        {
            let (
                token_a_key,
                mut token_a_account,
                token_b_key,
                mut token_b_account,
                pool_key,
                mut pool_account,
            ) = accounts.setup_token_accounts(
                &user_key,
                &withdrawer_key,
                initial_a,
                initial_b,
                withdraw_amount.try_into().unwrap(),
            );
            assert_eq!(
                Err(TokenError::MintMismatch.into()),
                accounts.withdraw_all_token_types(
                    &withdrawer_key,
                    &pool_key,
                    &mut pool_account,
                    &token_b_key,
                    &mut token_b_account,
                    &token_a_key,
                    &mut token_a_account,
                    withdraw_amount.try_into().unwrap(),
                    minimum_token_a_amount,
                    minimum_token_b_amount,
                )
            );
        }

        // wrong pool token account
        {
            let (
                token_a_key,
                mut token_a_account,
                token_b_key,
                mut token_b_account,
                _pool_key,
                _pool_account,
            ) = accounts.setup_token_accounts(
                &user_key,
                &withdrawer_key,
                initial_a,
                initial_b,
                withdraw_amount.try_into().unwrap(),
            );
            let (
                wrong_token_a_key,
                mut wrong_token_a_account,
                _token_b_key,
                _token_b_account,
                _pool_key,
                _pool_account,
            ) = accounts.setup_token_accounts(
                &user_key,
                &withdrawer_key,
                withdraw_amount.try_into().unwrap(),
                initial_b,
                withdraw_amount.try_into().unwrap(),
            );
            assert_eq!(
                Err(TokenError::MintMismatch.into()),
                accounts.withdraw_all_token_types(
                    &withdrawer_key,
                    &wrong_token_a_key,
                    &mut wrong_token_a_account,
                    &token_a_key,
                    &mut token_a_account,
                    &token_b_key,
                    &mut token_b_account,
                    withdraw_amount.try_into().unwrap(),
                    minimum_token_a_amount,
                    minimum_token_b_amount,
                )
            );
        }

        // wrong pool fee account
        {
            let (
                token_a_key,
                mut token_a_account,
                token_b_key,
                mut token_b_account,
                wrong_pool_key,
                wrong_pool_account,
            ) = accounts.setup_token_accounts(
                &user_key,
                &withdrawer_key,
                initial_a,
                initial_b,
                withdraw_amount.try_into().unwrap(),
            );
            let (
                _token_a_key,
                _token_a_account,
                _token_b_key,
                _token_b_account,
                pool_key,
                mut pool_account,
            ) = accounts.setup_token_accounts(
                &user_key,
                &withdrawer_key,
                initial_a,
                initial_b,
                withdraw_amount.try_into().unwrap(),
            );
            let old_pool_fee_account = accounts.pool_fee_account;
            let old_pool_fee_key = accounts.pool_fee_key;
            accounts.pool_fee_account = wrong_pool_account;
            accounts.pool_fee_key = wrong_pool_key;
            assert_eq!(
                Err(SwapError::IncorrectFeeAccount.into()),
                accounts.withdraw_all_token_types(
                    &withdrawer_key,
                    &pool_key,
                    &mut pool_account,
                    &token_a_key,
                    &mut token_a_account,
                    &token_b_key,
                    &mut token_b_account,
                    withdraw_amount.try_into().unwrap(),
                    minimum_token_a_amount,
                    minimum_token_b_amount,
                ),
            );
            accounts.pool_fee_account = old_pool_fee_account;
            accounts.pool_fee_key = old_pool_fee_key;
        }

        // no approval
        {
            let (
                token_a_key,
                mut token_a_account,
                token_b_key,
                mut token_b_account,
                pool_key,
                mut pool_account,
            ) = accounts.setup_token_accounts(
                &user_key,
                &withdrawer_key,
                0,
                0,
                withdraw_amount.try_into().unwrap(),
            );
            let user_transfer_authority_key = Pubkey::new_unique();
            assert_eq!(
                Err(TokenError::OwnerMismatch.into()),
                do_process_instruction(
                    withdraw_all_token_types(
                        &SWAP_PROGRAM_ID,
                        &TOKEN_PROGRAM_ID,
                        &accounts.swap_key,
                        &accounts.authority_key,
                        &user_transfer_authority_key,
                        &accounts.pool_mint_key,
                        &accounts.pool_fee_key,
                        &pool_key,
                        &accounts.token_a_key,
                        &accounts.token_b_key,
                        &token_a_key,
                        &token_b_key,
                        WithdrawAllTokenTypes {
                            pool_token_amount: withdraw_amount.try_into().unwrap(),
                            minimum_token_a_amount,
                            minimum_token_b_amount,
                        },
                    )
                        .unwrap(),
                    vec![
                        &mut accounts.swap_account,
                        &mut Account::default(),
                        &mut Account::default(),
                        &mut accounts.pool_mint_account,
                        &mut pool_account,
                        &mut accounts.token_a_account,
                        &mut accounts.token_b_account,
                        &mut token_a_account,
                        &mut token_b_account,
                        &mut accounts.pool_fee_account,
                        &mut Account::default(),
                    ],
                )
            );
        }

        // wrong token program id
        {
            let (
                token_a_key,
                mut token_a_account,
                token_b_key,
                mut token_b_account,
                pool_key,
                mut pool_account,
            ) = accounts.setup_token_accounts(
                &user_key,
                &withdrawer_key,
                initial_a,
                initial_b,
                withdraw_amount.try_into().unwrap(),
            );
            let wrong_key = Pubkey::new_unique();
            assert_eq!(
                Err(SwapError::IncorrectTokenProgramId.into()),
                do_process_instruction(
                    withdraw_all_token_types(
                        &SWAP_PROGRAM_ID,
                        &wrong_key,
                        &accounts.swap_key,
                        &accounts.authority_key,
                        &accounts.authority_key,
                        &accounts.pool_mint_key,
                        &accounts.pool_fee_key,
                        &pool_key,
                        &accounts.token_a_key,
                        &accounts.token_b_key,
                        &token_a_key,
                        &token_b_key,
                        WithdrawAllTokenTypes {
                            pool_token_amount: withdraw_amount.try_into().unwrap(),
                            minimum_token_a_amount,
                            minimum_token_b_amount,
                        },
                    )
                        .unwrap(),
                    vec![
                        &mut accounts.swap_account,
                        &mut Account::default(),
                        &mut Account::default(),
                        &mut accounts.pool_mint_account,
                        &mut pool_account,
                        &mut accounts.token_a_account,
                        &mut accounts.token_b_account,
                        &mut token_a_account,
                        &mut token_b_account,
                        &mut accounts.pool_fee_account,
                        &mut Account::default(),
                    ],
                )
            );
        }

        // wrong swap token accounts
        {
            let (
                token_a_key,
                mut token_a_account,
                token_b_key,
                mut token_b_account,
                pool_key,
                mut pool_account,
            ) = accounts.setup_token_accounts(
                &user_key,
                &withdrawer_key,
                initial_a,
                initial_b,
                initial_pool.try_into().unwrap(),
            );

            let old_a_key = accounts.token_a_key;
            let old_a_account = accounts.token_a_account;

            accounts.token_a_key = token_a_key;
            accounts.token_a_account = token_a_account.clone();

            // wrong swap token a account
            assert_eq!(
                Err(SwapError::IncorrectSwapAccount.into()),
                accounts.withdraw_all_token_types(
                    &withdrawer_key,
                    &pool_key,
                    &mut pool_account,
                    &token_a_key,
                    &mut token_a_account,
                    &token_b_key,
                    &mut token_b_account,
                    withdraw_amount.try_into().unwrap(),
                    minimum_token_a_amount,
                    minimum_token_b_amount,
                )
            );

            accounts.token_a_key = old_a_key;
            accounts.token_a_account = old_a_account;

            let old_b_key = accounts.token_b_key;
            let old_b_account = accounts.token_b_account;

            accounts.token_b_key = token_b_key;
            accounts.token_b_account = token_b_account.clone();

            // wrong swap token b account
            assert_eq!(
                Err(SwapError::IncorrectSwapAccount.into()),
                accounts.withdraw_all_token_types(
                    &withdrawer_key,
                    &pool_key,
                    &mut pool_account,
                    &token_a_key,
                    &mut token_a_account,
                    &token_b_key,
                    &mut token_b_account,
                    withdraw_amount.try_into().unwrap(),
                    minimum_token_a_amount,
                    minimum_token_b_amount,
                )
            );

            accounts.token_b_key = old_b_key;
            accounts.token_b_account = old_b_account;
        }

        // wrong mint
        {
            let (
                token_a_key,
                mut token_a_account,
                token_b_key,
                mut token_b_account,
                pool_key,
                mut pool_account,
            ) = accounts.setup_token_accounts(
                &user_key,
                &withdrawer_key,
                initial_a,
                initial_b,
                initial_pool.try_into().unwrap(),
            );
            let (pool_mint_key, pool_mint_account) =
                create_mint(&TOKEN_PROGRAM_ID, &accounts.authority_key, None);
            let old_pool_key = accounts.pool_mint_key;
            let old_pool_account = accounts.pool_mint_account;
            accounts.pool_mint_key = pool_mint_key;
            accounts.pool_mint_account = pool_mint_account;

            assert_eq!(
                Err(SwapError::IncorrectPoolMint.into()),
                accounts.withdraw_all_token_types(
                    &withdrawer_key,
                    &pool_key,
                    &mut pool_account,
                    &token_a_key,
                    &mut token_a_account,
                    &token_b_key,
                    &mut token_b_account,
                    withdraw_amount.try_into().unwrap(),
                    minimum_token_a_amount,
                    minimum_token_b_amount,
                )
            );

            accounts.pool_mint_key = old_pool_key;
            accounts.pool_mint_account = old_pool_account;
        }

        // withdrawing 1 pool token fails because it equates to 0 output tokens
        {
            let (
                token_a_key,
                mut token_a_account,
                token_b_key,
                mut token_b_account,
                pool_key,
                mut pool_account,
            ) = accounts.setup_token_accounts(
                &user_key,
                &withdrawer_key,
                initial_a,
                initial_b,
                initial_pool.try_into().unwrap(),
            );
            assert_eq!(
                Err(SwapError::ZeroTradingTokens.into()),
                accounts.withdraw_all_token_types(
                    &withdrawer_key,
                    &pool_key,
                    &mut pool_account,
                    &token_a_key,
                    &mut token_a_account,
                    &token_b_key,
                    &mut token_b_account,
                    1,
                    0,
                    0,
                )
            );
        }

        // slippage exceeded
        {
            let (
                token_a_key,
                mut token_a_account,
                token_b_key,
                mut token_b_account,
                pool_key,
                mut pool_account,
            ) = accounts.setup_token_accounts(
                &user_key,
                &withdrawer_key,
                initial_a,
                initial_b,
                initial_pool.try_into().unwrap(),
            );
            // minimum A amount out too high
            assert_eq!(
                Err(SwapError::ExceededSlippage.into()),
                accounts.withdraw_all_token_types(
                    &withdrawer_key,
                    &pool_key,
                    &mut pool_account,
                    &token_a_key,
                    &mut token_a_account,
                    &token_b_key,
                    &mut token_b_account,
                    withdraw_amount.try_into().unwrap(),
                    minimum_token_a_amount * 10,
                    minimum_token_b_amount,
                )
            );
            // minimum B amount out too high
            assert_eq!(
                Err(SwapError::ExceededSlippage.into()),
                accounts.withdraw_all_token_types(
                    &withdrawer_key,
                    &pool_key,
                    &mut pool_account,
                    &token_a_key,
                    &mut token_a_account,
                    &token_b_key,
                    &mut token_b_account,
                    withdraw_amount.try_into().unwrap(),
                    minimum_token_a_amount,
                    minimum_token_b_amount * 10,
                )
            );
        }

        // invalid input: can't use swap pool tokens as destination
        {
            let (
                token_a_key,
                mut token_a_account,
                token_b_key,
                mut token_b_account,
                pool_key,
                mut pool_account,
            ) = accounts.setup_token_accounts(
                &user_key,
                &withdrawer_key,
                initial_a,
                initial_b,
                initial_pool.try_into().unwrap(),
            );
            let swap_token_a_key = accounts.token_a_key;
            let mut swap_token_a_account = accounts.get_token_account(&swap_token_a_key).clone();
            assert_eq!(
                Err(SwapError::InvalidInput.into()),
                accounts.withdraw_all_token_types(
                    &withdrawer_key,
                    &pool_key,
                    &mut pool_account,
                    &swap_token_a_key,
                    &mut swap_token_a_account,
                    &token_b_key,
                    &mut token_b_account,
                    withdraw_amount.try_into().unwrap(),
                    minimum_token_a_amount,
                    minimum_token_b_amount,
                )
            );
            let swap_token_b_key = accounts.token_b_key;
            let mut swap_token_b_account = accounts.get_token_account(&swap_token_b_key).clone();
            assert_eq!(
                Err(SwapError::InvalidInput.into()),
                accounts.withdraw_all_token_types(
                    &withdrawer_key,
                    &pool_key,
                    &mut pool_account,
                    &token_a_key,
                    &mut token_a_account,
                    &swap_token_b_key,
                    &mut swap_token_b_account,
                    withdraw_amount.try_into().unwrap(),
                    minimum_token_a_amount,
                    minimum_token_b_amount,
                )
            );
        }

        // correct withdrawal
        {
            let (
                token_a_key,
                mut token_a_account,
                token_b_key,
                mut token_b_account,
                pool_key,
                mut pool_account,
            ) = accounts.setup_token_accounts(
                &user_key,
                &withdrawer_key,
                initial_a,
                initial_b,
                initial_pool.try_into().unwrap(),
            );

            accounts
                .withdraw_all_token_types(
                    &withdrawer_key,
                    &pool_key,
                    &mut pool_account,
                    &token_a_key,
                    &mut token_a_account,
                    &token_b_key,
                    &mut token_b_account,
                    withdraw_amount.try_into().unwrap(),
                    minimum_token_a_amount,
                    minimum_token_b_amount,
                )
                .unwrap();

            let swap_token_a =
                spl_token::state::Account::unpack(&accounts.token_a_account.data).unwrap();
            let swap_token_b =
                spl_token::state::Account::unpack(&accounts.token_b_account.data).unwrap();
            let pool_mint =
                spl_token::state::Mint::unpack(&accounts.pool_mint_account.data).unwrap();
            let withdraw_fee = accounts.fees.owner_withdraw_fee(withdraw_amount).unwrap();

            let results = accounts
                .swap_curve
                .calculator
                .pool_tokens_to_trading_tokens(
                    withdraw_amount - withdraw_fee,
                    pool_mint.supply.try_into().unwrap(),
                    swap_token_a.amount.try_into().unwrap(),
                    swap_token_b.amount.try_into().unwrap(),
                    RoundDirection::Floor,
                )
                .unwrap();
            assert_eq!(
                swap_token_a.amount,
                token_a_amount - to_u64(results.token_a_amount).unwrap()
            );
            assert_eq!(
                swap_token_b.amount,
                token_b_amount - to_u64(results.token_b_amount).unwrap()
            );
            let token_a = spl_token::state::Account::unpack(&token_a_account.data).unwrap();
            assert_eq!(
                token_a.amount,
                initial_a + to_u64(results.token_a_amount).unwrap()
            );
            let token_b = spl_token::state::Account::unpack(&token_b_account.data).unwrap();
            assert_eq!(
                token_b.amount,
                initial_b + to_u64(results.token_b_amount).unwrap()
            );
            let pool_account = spl_token::state::Account::unpack(&pool_account.data).unwrap();
            assert_eq!(
                pool_account.amount,
                to_u64(initial_pool - withdraw_amount).unwrap()
            );
            let fee_account =
                spl_token::state::Account::unpack(&accounts.pool_fee_account.data).unwrap();
            assert_eq!(
                fee_account.amount,
                TryInto::<u64>::try_into(withdraw_fee).unwrap()
            );
        }

        #[cfg(not(feature = "production"))]
            {
                let (
                    token_a_key,
                    mut token_a_account,
                    token_b_key,
                    mut token_b_account,
                    _pool_key,
                    mut _pool_account,
                ) = accounts.setup_token_accounts(&user_key, &withdrawer_key, 0, 0, 0);

                let pool_fee_key = accounts.pool_fee_key;
                let mut pool_fee_account = accounts.pool_fee_account.clone();
                let fee_account = spl_token::state::Account::unpack(&pool_fee_account.data).unwrap();
                let pool_fee_amount = fee_account.amount;
                let fee_owner = fee_account.owner;
                accounts
                    .withdraw_all_token_types(
                        &fee_owner,
                        &pool_fee_key,
                        &mut pool_fee_account,
                        &token_a_key,
                        &mut token_a_account,
                        &token_b_key,
                        &mut token_b_account,
                        pool_fee_amount,
                        0,
                        0,
                    ).unwrap();

                let swap_token_a =
                    spl_token::state::Account::unpack(&accounts.token_a_account.data).unwrap();
                let swap_token_b =
                    spl_token::state::Account::unpack(&accounts.token_b_account.data).unwrap();
                let pool_mint =
                    spl_token::state::Mint::unpack(&accounts.pool_mint_account.data).unwrap();
                let results = accounts
                    .swap_curve
                    .calculator
                    .pool_tokens_to_trading_tokens(
                        pool_fee_amount.try_into().unwrap(),
                        pool_mint.supply.try_into().unwrap(),
                        swap_token_a.amount.try_into().unwrap(),
                        swap_token_b.amount.try_into().unwrap(),
                        RoundDirection::Floor,
                    )
                    .unwrap();
                let token_a = spl_token::state::Account::unpack(&token_a_account.data).unwrap();
                assert_eq!(
                    token_a.amount,
                    TryInto::<u64>::try_into(results.token_a_amount).unwrap()
                );
                let token_b = spl_token::state::Account::unpack(&token_b_account.data).unwrap();
                assert_eq!(
                    token_b.amount,
                    TryInto::<u64>::try_into(results.token_b_amount).unwrap()
                );
            }
    }

    #[test]
    fn test_deposit_one_exact_in() {
        let user_key = Pubkey::new_unique();
        let depositor_key = Pubkey::new_unique();
        let trade_fee_numerator = 25;
        let trade_fee_denominator = 10000;
        let owner_trade_fee_numerator = 5;
        let owner_trade_fee_denominator = 10000;
        let owner_withdraw_fee_numerator = 0;
        let owner_withdraw_fee_denominator = 0;
        let host_fee_numerator = 0;
        let host_fee_denominator = 0;

        let fees = Fees {
            trade_fee_numerator,
            trade_fee_denominator,
            owner_trade_fee_numerator,
            owner_trade_fee_denominator,
            owner_withdraw_fee_numerator,
            owner_withdraw_fee_denominator,
            host_fee_numerator,
            host_fee_denominator,
        };

        let token_a_amount = 1000;
        let token_b_amount = 9000;
        let curve_type = CurveType::ConstantProduct;
        let swap_curve = SwapCurve {
            curve_type,
            calculator: Box::new(ConstantProductCurve {}),
        };

        let mut accounts =
            SwapAccountInfo::new(&user_key, fees, swap_curve, token_a_amount, token_b_amount);

        let deposit_a = token_a_amount / 10;
        let deposit_b = token_b_amount / 10;
        let pool_amount = to_u64(INITIAL_SWAP_POOL_AMOUNT / 100).unwrap();

        // swap not initialized
        {
            let (
                token_a_key,
                mut token_a_account,
                _token_b_key,
                _token_b_account,
                pool_key,
                mut pool_account,
            ) = accounts.setup_token_accounts(&user_key, &depositor_key, deposit_a, deposit_b, 0);
            assert_eq!(
                Err(ProgramError::UninitializedAccount),
                accounts.deposit_single_token_type_exact_amount_in(
                    &depositor_key,
                    &token_a_key,
                    &mut token_a_account,
                    &pool_key,
                    &mut pool_account,
                    deposit_a,
                    pool_amount,
                )
            );
        }

        accounts.initialize_swap().unwrap();

        // wrong owner for swap account
        {
            let (
                token_a_key,
                mut token_a_account,
                _token_b_key,
                _token_b_account,
                pool_key,
                mut pool_account,
            ) = accounts.setup_token_accounts(&user_key, &depositor_key, deposit_a, deposit_b, 0);
            let old_swap_account = accounts.swap_account;
            let mut wrong_swap_account = old_swap_account.clone();
            wrong_swap_account.owner = TOKEN_PROGRAM_ID;
            accounts.swap_account = wrong_swap_account;
            assert_eq!(
                Err(ProgramError::IncorrectProgramId),
                accounts.deposit_single_token_type_exact_amount_in(
                    &depositor_key,
                    &token_a_key,
                    &mut token_a_account,
                    &pool_key,
                    &mut pool_account,
                    deposit_a,
                    pool_amount,
                )
            );
            accounts.swap_account = old_swap_account;
        }

        // wrong nonce for authority_key
        {
            let (
                token_a_key,
                mut token_a_account,
                _token_b_key,
                _token_b_account,
                pool_key,
                mut pool_account,
            ) = accounts.setup_token_accounts(&user_key, &depositor_key, deposit_a, deposit_b, 0);
            let old_authority = accounts.authority_key;
            let (bad_authority_key, _nonce) = Pubkey::find_program_address(
                &[&accounts.swap_key.to_bytes()[..]],
                &TOKEN_PROGRAM_ID,
            );
            accounts.authority_key = bad_authority_key;
            assert_eq!(
                Err(SwapError::InvalidProgramAddress.into()),
                accounts.deposit_single_token_type_exact_amount_in(
                    &depositor_key,
                    &token_a_key,
                    &mut token_a_account,
                    &pool_key,
                    &mut pool_account,
                    deposit_a,
                    pool_amount,
                )
            );
            accounts.authority_key = old_authority;
        }

        // not enough token A / B
        {
            let (
                token_a_key,
                mut token_a_account,
                token_b_key,
                mut token_b_account,
                pool_key,
                mut pool_account,
            ) = accounts.setup_token_accounts(
                &user_key,
                &depositor_key,
                deposit_a / 2,
                deposit_b / 2,
                0,
            );
            assert_eq!(
                Err(TokenError::InsufficientFunds.into()),
                accounts.deposit_single_token_type_exact_amount_in(
                    &depositor_key,
                    &token_a_key,
                    &mut token_a_account,
                    &pool_key,
                    &mut pool_account,
                    deposit_a,
                    0,
                )
            );
            assert_eq!(
                Err(TokenError::InsufficientFunds.into()),
                accounts.deposit_single_token_type_exact_amount_in(
                    &depositor_key,
                    &token_b_key,
                    &mut token_b_account,
                    &pool_key,
                    &mut pool_account,
                    deposit_b,
                    0,
                )
            );
        }

        // wrong pool token account
        {
            let (
                token_a_key,
                mut token_a_account,
                token_b_key,
                mut token_b_account,
                _pool_key,
                mut _pool_account,
            ) = accounts.setup_token_accounts(&user_key, &depositor_key, deposit_a, deposit_b, 0);
            assert_eq!(
                Err(TokenError::MintMismatch.into()),
                accounts.deposit_single_token_type_exact_amount_in(
                    &depositor_key,
                    &token_a_key,
                    &mut token_a_account,
                    &token_b_key,
                    &mut token_b_account,
                    deposit_a,
                    pool_amount,
                )
            );
        }

        // no approval
        {
            let (
                token_a_key,
                mut token_a_account,
                _token_b_key,
                _token_b_account,
                pool_key,
                mut pool_account,
            ) = accounts.setup_token_accounts(&user_key, &depositor_key, deposit_a, deposit_b, 0);
            let user_transfer_authority_key = Pubkey::new_unique();
            assert_eq!(
                Err(TokenError::OwnerMismatch.into()),
                do_process_instruction(
                    deposit_single_token_type_exact_amount_in(
                        &SWAP_PROGRAM_ID,
                        &TOKEN_PROGRAM_ID,
                        &accounts.swap_key,
                        &accounts.authority_key,
                        &user_transfer_authority_key,
                        &token_a_key,
                        &accounts.token_a_key,
                        &accounts.token_b_key,
                        &accounts.pool_mint_key,
                        &pool_key,
                        DepositSingleTokenTypeExactAmountIn {
                            source_token_amount: deposit_a,
                            minimum_pool_token_amount: pool_amount,
                        },
                    )
                        .unwrap(),
                    vec![
                        &mut accounts.swap_account,
                        &mut Account::default(),
                        &mut Account::default(),
                        &mut token_a_account,
                        &mut accounts.token_a_account,
                        &mut accounts.token_b_account,
                        &mut accounts.pool_mint_account,
                        &mut pool_account,
                        &mut Account::default(),
                    ],
                )
            );
        }

        // wrong token program id
        {
            let (
                token_a_key,
                mut token_a_account,
                _token_b_key,
                _token_b_account,
                pool_key,
                mut pool_account,
            ) = accounts.setup_token_accounts(&user_key, &depositor_key, deposit_a, deposit_b, 0);
            let wrong_key = Pubkey::new_unique();
            assert_eq!(
                Err(SwapError::IncorrectTokenProgramId.into()),
                do_process_instruction(
                    deposit_single_token_type_exact_amount_in(
                        &SWAP_PROGRAM_ID,
                        &wrong_key,
                        &accounts.swap_key,
                        &accounts.authority_key,
                        &accounts.authority_key,
                        &token_a_key,
                        &accounts.token_a_key,
                        &accounts.token_b_key,
                        &accounts.pool_mint_key,
                        &pool_key,
                        DepositSingleTokenTypeExactAmountIn {
                            source_token_amount: deposit_a,
                            minimum_pool_token_amount: pool_amount,
                        },
                    )
                        .unwrap(),
                    vec![
                        &mut accounts.swap_account,
                        &mut Account::default(),
                        &mut Account::default(),
                        &mut token_a_account,
                        &mut accounts.token_a_account,
                        &mut accounts.token_b_account,
                        &mut accounts.pool_mint_account,
                        &mut pool_account,
                        &mut Account::default(),
                    ],
                )
            );
        }

        // wrong swap token accounts
        {
            let (
                token_a_key,
                mut token_a_account,
                token_b_key,
                token_b_account,
                pool_key,
                mut pool_account,
            ) = accounts.setup_token_accounts(&user_key, &depositor_key, deposit_a, deposit_b, 0);

            let old_a_key = accounts.token_a_key;
            let old_a_account = accounts.token_a_account;

            accounts.token_a_key = token_a_key;
            accounts.token_a_account = token_a_account.clone();

            // wrong swap token a account
            assert_eq!(
                Err(SwapError::IncorrectSwapAccount.into()),
                accounts.deposit_single_token_type_exact_amount_in(
                    &depositor_key,
                    &token_a_key,
                    &mut token_a_account,
                    &pool_key,
                    &mut pool_account,
                    deposit_a,
                    pool_amount,
                )
            );

            accounts.token_a_key = old_a_key;
            accounts.token_a_account = old_a_account;

            let old_b_key = accounts.token_b_key;
            let old_b_account = accounts.token_b_account;

            accounts.token_b_key = token_b_key;
            accounts.token_b_account = token_b_account;

            // wrong swap token b account
            assert_eq!(
                Err(SwapError::IncorrectSwapAccount.into()),
                accounts.deposit_single_token_type_exact_amount_in(
                    &depositor_key,
                    &token_a_key,
                    &mut token_a_account,
                    &pool_key,
                    &mut pool_account,
                    deposit_a,
                    pool_amount,
                )
            );

            accounts.token_b_key = old_b_key;
            accounts.token_b_account = old_b_account;
        }

        // wrong mint
        {
            let (
                token_a_key,
                mut token_a_account,
                _token_b_key,
                _token_b_account,
                pool_key,
                mut pool_account,
            ) = accounts.setup_token_accounts(&user_key, &depositor_key, deposit_a, deposit_b, 0);
            let (pool_mint_key, pool_mint_account) =
                create_mint(&TOKEN_PROGRAM_ID, &accounts.authority_key, None);
            let old_pool_key = accounts.pool_mint_key;
            let old_pool_account = accounts.pool_mint_account;
            accounts.pool_mint_key = pool_mint_key;
            accounts.pool_mint_account = pool_mint_account;

            assert_eq!(
                Err(SwapError::IncorrectPoolMint.into()),
                accounts.deposit_single_token_type_exact_amount_in(
                    &depositor_key,
                    &token_a_key,
                    &mut token_a_account,
                    &pool_key,
                    &mut pool_account,
                    deposit_a,
                    pool_amount,
                )
            );

            accounts.pool_mint_key = old_pool_key;
            accounts.pool_mint_account = old_pool_account;
        }

        // slippage exceeded
        {
            let (
                token_a_key,
                mut token_a_account,
                token_b_key,
                mut token_b_account,
                pool_key,
                mut pool_account,
            ) = accounts.setup_token_accounts(&user_key, &depositor_key, deposit_a, deposit_b, 0);
            // minimum pool amount too high
            assert_eq!(
                Err(SwapError::ExceededSlippage.into()),
                accounts.deposit_single_token_type_exact_amount_in(
                    &depositor_key,
                    &token_a_key,
                    &mut token_a_account,
                    &pool_key,
                    &mut pool_account,
                    deposit_a / 10,
                    pool_amount,
                )
            );
            // minimum pool amount too high
            assert_eq!(
                Err(SwapError::ExceededSlippage.into()),
                accounts.deposit_single_token_type_exact_amount_in(
                    &depositor_key,
                    &token_b_key,
                    &mut token_b_account,
                    &pool_key,
                    &mut pool_account,
                    deposit_b / 10,
                    pool_amount,
                )
            );
        }

        // invalid input: can't use swap pool tokens as source
        {
            let (
                _token_a_key,
                _token_a_account,
                _token_b_key,
                _token_b_account,
                pool_key,
                mut pool_account,
            ) = accounts.setup_token_accounts(&user_key, &depositor_key, deposit_a, deposit_b, 0);
            let swap_token_a_key = accounts.token_a_key;
            let mut swap_token_a_account = accounts.get_token_account(&swap_token_a_key).clone();
            let swap_token_b_key = accounts.token_b_key;
            let mut swap_token_b_account = accounts.get_token_account(&swap_token_b_key).clone();
            let authority_key = accounts.authority_key;
            assert_eq!(
                Err(SwapError::InvalidInput.into()),
                accounts.deposit_single_token_type_exact_amount_in(
                    &authority_key,
                    &swap_token_a_key,
                    &mut swap_token_a_account,
                    &pool_key,
                    &mut pool_account,
                    deposit_a,
                    pool_amount,
                )
            );
            assert_eq!(
                Err(SwapError::InvalidInput.into()),
                accounts.deposit_single_token_type_exact_amount_in(
                    &authority_key,
                    &swap_token_b_key,
                    &mut swap_token_b_account,
                    &pool_key,
                    &mut pool_account,
                    deposit_b,
                    pool_amount,
                )
            );
        }

        // correctly deposit
        {
            let (
                token_a_key,
                mut token_a_account,
                token_b_key,
                mut token_b_account,
                pool_key,
                mut pool_account,
            ) = accounts.setup_token_accounts(&user_key, &depositor_key, deposit_a, deposit_b, 0);
            accounts
                .deposit_single_token_type_exact_amount_in(
                    &depositor_key,
                    &token_a_key,
                    &mut token_a_account,
                    &pool_key,
                    &mut pool_account,
                    deposit_a,
                    pool_amount,
                )
                .unwrap();

            let swap_token_a =
                spl_token::state::Account::unpack(&accounts.token_a_account.data).unwrap();
            assert_eq!(swap_token_a.amount, deposit_a + token_a_amount);

            let token_a = spl_token::state::Account::unpack(&token_a_account.data).unwrap();
            assert_eq!(token_a.amount, 0);

            accounts
                .deposit_single_token_type_exact_amount_in(
                    &depositor_key,
                    &token_b_key,
                    &mut token_b_account,
                    &pool_key,
                    &mut pool_account,
                    deposit_b,
                    pool_amount,
                )
                .unwrap();
            let swap_token_b =
                spl_token::state::Account::unpack(&accounts.token_b_account.data).unwrap();
            assert_eq!(swap_token_b.amount, deposit_b + token_b_amount);

            let token_b = spl_token::state::Account::unpack(&token_b_account.data).unwrap();
            assert_eq!(token_b.amount, 0);

            let pool_account = spl_token::state::Account::unpack(&pool_account.data).unwrap();
            let swap_pool_account =
                spl_token::state::Account::unpack(&accounts.pool_token_account.data).unwrap();
            let pool_mint =
                spl_token::state::Mint::unpack(&accounts.pool_mint_account.data).unwrap();
            assert_eq!(
                pool_mint.supply,
                pool_account.amount + swap_pool_account.amount
            );
        }
    }

    #[test]
    fn test_withdraw_one_exact_out() {
        let user_key = Pubkey::new_unique();
        let trade_fee_numerator = 25;
        let trade_fee_denominator = 10000;
        let owner_trade_fee_numerator = 5;
        let owner_trade_fee_denominator = 10000;
        let owner_withdraw_fee_numerator = 0;
        let owner_withdraw_fee_denominator = 0;
        let host_fee_numerator = 0;
        let host_fee_denominator = 0;

        let fees = Fees {
            trade_fee_numerator,
            trade_fee_denominator,
            owner_trade_fee_numerator,
            owner_trade_fee_denominator,
            owner_withdraw_fee_numerator,
            owner_withdraw_fee_denominator,
            host_fee_numerator,
            host_fee_denominator,
        };

        let token_a_amount = 100_000;
        let token_b_amount = 200_000;
        let curve_type = CurveType::ConstantProduct;
        let swap_curve = SwapCurve {
            curve_type,
            calculator: Box::new(ConstantProductCurve {}),
        };

        let withdrawer_key = Pubkey::new_unique();
        let initial_a = token_a_amount / 10;
        let initial_b = token_b_amount / 10;
        let initial_pool = swap_curve.calculator.new_pool_supply() / 10;
        let maximum_pool_token_amount = to_u64(initial_pool / 4).unwrap();
        let destination_a_amount = initial_a / 40;
        let destination_b_amount = initial_b / 40;

        let mut accounts =
            SwapAccountInfo::new(&user_key, fees, swap_curve, token_a_amount, token_b_amount);

        // swap not initialized
        {
            let (
                token_a_key,
                mut token_a_account,
                _token_b_key,
                _token_b_account,
                pool_key,
                mut pool_account,
            ) = accounts.setup_token_accounts(&user_key, &withdrawer_key, initial_a, initial_b, 0);
            assert_eq!(
                Err(ProgramError::UninitializedAccount),
                accounts.withdraw_single_token_type_exact_amount_out(
                    &withdrawer_key,
                    &pool_key,
                    &mut pool_account,
                    &token_a_key,
                    &mut token_a_account,
                    destination_a_amount,
                    maximum_pool_token_amount,
                )
            );
        }

        accounts.initialize_swap().unwrap();

        // wrong owner for swap account
        {
            let (
                token_a_key,
                mut token_a_account,
                _token_b_key,
                _token_b_account,
                pool_key,
                mut pool_account,
            ) = accounts.setup_token_accounts(&user_key, &withdrawer_key, initial_a, initial_b, 0);
            let old_swap_account = accounts.swap_account;
            let mut wrong_swap_account = old_swap_account.clone();
            wrong_swap_account.owner = TOKEN_PROGRAM_ID;
            accounts.swap_account = wrong_swap_account;
            assert_eq!(
                Err(ProgramError::IncorrectProgramId),
                accounts.withdraw_single_token_type_exact_amount_out(
                    &withdrawer_key,
                    &pool_key,
                    &mut pool_account,
                    &token_a_key,
                    &mut token_a_account,
                    destination_a_amount,
                    maximum_pool_token_amount,
                )
            );
            accounts.swap_account = old_swap_account;
        }

        // wrong nonce for authority_key
        {
            let (
                _token_a_key,
                _token_a_account,
                token_b_key,
                mut token_b_account,
                pool_key,
                mut pool_account,
            ) = accounts.setup_token_accounts(&user_key, &withdrawer_key, initial_a, initial_b, 0);
            let old_authority = accounts.authority_key;
            let (bad_authority_key, _nonce) = Pubkey::find_program_address(
                &[&accounts.swap_key.to_bytes()[..]],
                &TOKEN_PROGRAM_ID,
            );
            accounts.authority_key = bad_authority_key;
            assert_eq!(
                Err(SwapError::InvalidProgramAddress.into()),
                accounts.withdraw_single_token_type_exact_amount_out(
                    &withdrawer_key,
                    &pool_key,
                    &mut pool_account,
                    &token_b_key,
                    &mut token_b_account,
                    destination_b_amount,
                    maximum_pool_token_amount,
                )
            );
            accounts.authority_key = old_authority;
        }

        // not enough pool tokens
        {
            let (
                _token_a_key,
                _token_a_account,
                token_b_key,
                mut token_b_account,
                pool_key,
                mut pool_account,
            ) = accounts.setup_token_accounts(
                &user_key,
                &withdrawer_key,
                initial_a,
                initial_b,
                maximum_pool_token_amount / 1000,
            );
            assert_eq!(
                Err(TokenError::InsufficientFunds.into()),
                accounts.withdraw_single_token_type_exact_amount_out(
                    &withdrawer_key,
                    &pool_key,
                    &mut pool_account,
                    &token_b_key,
                    &mut token_b_account,
                    destination_b_amount,
                    maximum_pool_token_amount,
                )
            );
        }

        // wrong pool token account
        {
            let (
                token_a_key,
                mut token_a_account,
                token_b_key,
                mut token_b_account,
                _pool_key,
                _pool_account,
            ) = accounts.setup_token_accounts(
                &user_key,
                &withdrawer_key,
                maximum_pool_token_amount,
                initial_b,
                maximum_pool_token_amount,
            );
            assert_eq!(
                Err(TokenError::MintMismatch.into()),
                accounts.withdraw_single_token_type_exact_amount_out(
                    &withdrawer_key,
                    &token_a_key,
                    &mut token_a_account,
                    &token_b_key,
                    &mut token_b_account,
                    destination_b_amount,
                    maximum_pool_token_amount,
                )
            );
        }

        // wrong pool fee account
        {
            let (
                token_a_key,
                mut token_a_account,
                _token_b_key,
                _token_b_account,
                wrong_pool_key,
                wrong_pool_account,
            ) = accounts.setup_token_accounts(
                &user_key,
                &withdrawer_key,
                initial_a,
                initial_b,
                maximum_pool_token_amount,
            );
            let (
                _token_a_key,
                _token_a_account,
                _token_b_key,
                _token_b_account,
                pool_key,
                mut pool_account,
            ) = accounts.setup_token_accounts(
                &user_key,
                &withdrawer_key,
                initial_a,
                initial_b,
                maximum_pool_token_amount,
            );
            let old_pool_fee_account = accounts.pool_fee_account;
            let old_pool_fee_key = accounts.pool_fee_key;
            accounts.pool_fee_account = wrong_pool_account;
            accounts.pool_fee_key = wrong_pool_key;
            assert_eq!(
                Err(SwapError::IncorrectFeeAccount.into()),
                accounts.withdraw_single_token_type_exact_amount_out(
                    &withdrawer_key,
                    &pool_key,
                    &mut pool_account,
                    &token_a_key,
                    &mut token_a_account,
                    destination_a_amount,
                    maximum_pool_token_amount,
                )
            );
            accounts.pool_fee_account = old_pool_fee_account;
            accounts.pool_fee_key = old_pool_fee_key;
        }

        // no approval
        {
            let (
                token_a_key,
                mut token_a_account,
                _token_b_key,
                _token_b_account,
                pool_key,
                mut pool_account,
            ) = accounts.setup_token_accounts(
                &user_key,
                &withdrawer_key,
                0,
                0,
                maximum_pool_token_amount,
            );
            let user_transfer_authority_key = Pubkey::new_unique();
            assert_eq!(
                Err(TokenError::OwnerMismatch.into()),
                do_process_instruction(
                    withdraw_single_token_type_exact_amount_out(
                        &SWAP_PROGRAM_ID,
                        &TOKEN_PROGRAM_ID,
                        &accounts.swap_key,
                        &accounts.authority_key,
                        &user_transfer_authority_key,
                        &accounts.pool_mint_key,
                        &accounts.pool_fee_key,
                        &pool_key,
                        &accounts.token_a_key,
                        &accounts.token_b_key,
                        &token_a_key,
                        WithdrawSingleTokenTypeExactAmountOut {
                            destination_token_amount: destination_a_amount,
                            maximum_pool_token_amount,
                        },
                    )
                        .unwrap(),
                    vec![
                        &mut accounts.swap_account,
                        &mut Account::default(),
                        &mut Account::default(),
                        &mut accounts.pool_mint_account,
                        &mut pool_account,
                        &mut accounts.token_a_account,
                        &mut accounts.token_b_account,
                        &mut token_a_account,
                        &mut accounts.pool_fee_account,
                        &mut Account::default(),
                    ],
                )
            );
        }

        // wrong token program id
        {
            let (
                token_a_key,
                mut token_a_account,
                _token_b_key,
                _token_b_account,
                pool_key,
                mut pool_account,
            ) = accounts.setup_token_accounts(
                &user_key,
                &withdrawer_key,
                initial_a,
                initial_b,
                maximum_pool_token_amount,
            );
            let wrong_key = Pubkey::new_unique();
            assert_eq!(
                Err(SwapError::IncorrectTokenProgramId.into()),
                do_process_instruction(
                    withdraw_single_token_type_exact_amount_out(
                        &SWAP_PROGRAM_ID,
                        &wrong_key,
                        &accounts.swap_key,
                        &accounts.authority_key,
                        &accounts.authority_key,
                        &accounts.pool_mint_key,
                        &accounts.pool_fee_key,
                        &pool_key,
                        &accounts.token_a_key,
                        &accounts.token_b_key,
                        &token_a_key,
                        WithdrawSingleTokenTypeExactAmountOut {
                            destination_token_amount: destination_a_amount,
                            maximum_pool_token_amount,
                        },
                    )
                        .unwrap(),
                    vec![
                        &mut accounts.swap_account,
                        &mut Account::default(),
                        &mut Account::default(),
                        &mut accounts.pool_mint_account,
                        &mut pool_account,
                        &mut accounts.token_a_account,
                        &mut accounts.token_b_account,
                        &mut token_a_account,
                        &mut accounts.pool_fee_account,
                        &mut Account::default(),
                    ],
                )
            );
        }

        // wrong swap token accounts
        {
            let (
                token_a_key,
                mut token_a_account,
                token_b_key,
                mut token_b_account,
                pool_key,
                mut pool_account,
            ) = accounts.setup_token_accounts(
                &user_key,
                &withdrawer_key,
                initial_a,
                initial_b,
                initial_pool.try_into().unwrap(),
            );

            let old_a_key = accounts.token_a_key;
            let old_a_account = accounts.token_a_account;

            accounts.token_a_key = token_a_key;
            accounts.token_a_account = token_a_account.clone();

            // wrong swap token a account
            assert_eq!(
                Err(SwapError::IncorrectSwapAccount.into()),
                accounts.withdraw_single_token_type_exact_amount_out(
                    &withdrawer_key,
                    &pool_key,
                    &mut pool_account,
                    &token_a_key,
                    &mut token_a_account,
                    destination_a_amount,
                    maximum_pool_token_amount,
                )
            );

            accounts.token_a_key = old_a_key;
            accounts.token_a_account = old_a_account;

            let old_b_key = accounts.token_b_key;
            let old_b_account = accounts.token_b_account;

            accounts.token_b_key = token_b_key;
            accounts.token_b_account = token_b_account.clone();

            // wrong swap token b account
            assert_eq!(
                Err(SwapError::IncorrectSwapAccount.into()),
                accounts.withdraw_single_token_type_exact_amount_out(
                    &withdrawer_key,
                    &pool_key,
                    &mut pool_account,
                    &token_b_key,
                    &mut token_b_account,
                    destination_b_amount,
                    maximum_pool_token_amount,
                )
            );

            accounts.token_b_key = old_b_key;
            accounts.token_b_account = old_b_account;
        }

        // wrong mint
        {
            let (
                token_a_key,
                mut token_a_account,
                _token_b_key,
                _token_b_account,
                pool_key,
                mut pool_account,
            ) = accounts.setup_token_accounts(
                &user_key,
                &withdrawer_key,
                initial_a,
                initial_b,
                initial_pool.try_into().unwrap(),
            );
            let (pool_mint_key, pool_mint_account) =
                create_mint(&TOKEN_PROGRAM_ID, &accounts.authority_key, None);
            let old_pool_key = accounts.pool_mint_key;
            let old_pool_account = accounts.pool_mint_account;
            accounts.pool_mint_key = pool_mint_key;
            accounts.pool_mint_account = pool_mint_account;

            assert_eq!(
                Err(SwapError::IncorrectPoolMint.into()),
                accounts.withdraw_single_token_type_exact_amount_out(
                    &withdrawer_key,
                    &pool_key,
                    &mut pool_account,
                    &token_a_key,
                    &mut token_a_account,
                    destination_a_amount,
                    maximum_pool_token_amount,
                )
            );

            accounts.pool_mint_key = old_pool_key;
            accounts.pool_mint_account = old_pool_account;
        }

        // slippage exceeded
        {
            let (
                token_a_key,
                mut token_a_account,
                token_b_key,
                mut token_b_account,
                pool_key,
                mut pool_account,
            ) = accounts.setup_token_accounts(
                &user_key,
                &withdrawer_key,
                initial_a,
                initial_b,
                maximum_pool_token_amount,
            );

            // maximum pool token amount too low
            assert_eq!(
                Err(SwapError::ExceededSlippage.into()),
                accounts.withdraw_single_token_type_exact_amount_out(
                    &withdrawer_key,
                    &pool_key,
                    &mut pool_account,
                    &token_a_key,
                    &mut token_a_account,
                    destination_a_amount,
                    maximum_pool_token_amount / 1000,
                )
            );
            assert_eq!(
                Err(SwapError::ExceededSlippage.into()),
                accounts.withdraw_single_token_type_exact_amount_out(
                    &withdrawer_key,
                    &pool_key,
                    &mut pool_account,
                    &token_b_key,
                    &mut token_b_account,
                    destination_b_amount,
                    maximum_pool_token_amount / 1000,
                )
            );
        }

        // invalid input: can't use swap pool tokens as destination
        {
            let (
                _token_a_key,
                _token_a_account,
                _token_b_key,
                _token_b_account,
                pool_key,
                mut pool_account,
            ) = accounts.setup_token_accounts(
                &user_key,
                &withdrawer_key,
                initial_a,
                initial_b,
                maximum_pool_token_amount,
            );
            let swap_token_a_key = accounts.token_a_key;
            let mut swap_token_a_account = accounts.get_token_account(&swap_token_a_key).clone();
            assert_eq!(
                Err(SwapError::InvalidInput.into()),
                accounts.withdraw_single_token_type_exact_amount_out(
                    &withdrawer_key,
                    &pool_key,
                    &mut pool_account,
                    &swap_token_a_key,
                    &mut swap_token_a_account,
                    destination_a_amount,
                    maximum_pool_token_amount,
                )
            );
            let swap_token_b_key = accounts.token_b_key;
            let mut swap_token_b_account = accounts.get_token_account(&swap_token_b_key).clone();
            assert_eq!(
                Err(SwapError::InvalidInput.into()),
                accounts.withdraw_single_token_type_exact_amount_out(
                    &withdrawer_key,
                    &pool_key,
                    &mut pool_account,
                    &swap_token_b_key,
                    &mut swap_token_b_account,
                    destination_b_amount,
                    maximum_pool_token_amount,
                )
            );
        }

        // correct withdrawal
        {
            let (
                token_a_key,
                mut token_a_account,
                _token_b_key,
                _token_b_account,
                pool_key,
                mut pool_account,
            ) = accounts.setup_token_accounts(
                &user_key,
                &withdrawer_key,
                initial_a,
                initial_b,
                initial_pool.try_into().unwrap(),
            );

            let swap_token_a =
                spl_token::state::Account::unpack(&accounts.token_a_account.data).unwrap();
            let swap_token_b =
                spl_token::state::Account::unpack(&accounts.token_b_account.data).unwrap();
            let pool_mint =
                spl_token::state::Mint::unpack(&accounts.pool_mint_account.data).unwrap();

            let pool_token_amount = accounts
                .swap_curve
                .trading_tokens_to_pool_tokens(
                    destination_a_amount.try_into().unwrap(),
                    (swap_token_a.amount - destination_a_amount)
                        .try_into()
                        .unwrap(),
                    swap_token_b.amount.try_into().unwrap(),
                    pool_mint.supply.try_into().unwrap(),
                    TradeDirection::AtoB,
                    RoundDirection::Ceiling,
                    &accounts.fees,
                )
                .unwrap();
            let withdraw_fee = accounts.fees.owner_withdraw_fee(pool_token_amount).unwrap();

            accounts
                .withdraw_single_token_type_exact_amount_out(
                    &withdrawer_key,
                    &pool_key,
                    &mut pool_account,
                    &token_a_key,
                    &mut token_a_account,
                    destination_a_amount,
                    maximum_pool_token_amount,
                )
                .unwrap();

            let swap_token_a =
                spl_token::state::Account::unpack(&accounts.token_a_account.data).unwrap();

            assert_eq!(swap_token_a.amount, token_a_amount - destination_a_amount);
            let token_a = spl_token::state::Account::unpack(&token_a_account.data).unwrap();
            assert_eq!(token_a.amount, initial_a + destination_a_amount);

            let pool_account = spl_token::state::Account::unpack(&pool_account.data).unwrap();
            assert_eq!(
                pool_account.amount,
                to_u64(initial_pool - pool_token_amount - withdraw_fee).unwrap()
            );
            let fee_account =
                spl_token::state::Account::unpack(&accounts.pool_fee_account.data).unwrap();
            assert_eq!(fee_account.amount, to_u64(withdraw_fee).unwrap());
        }

        // correct withdrawal from fee account
        #[cfg(not(feature = "production"))]
            {
                let (
                    token_a_key,
                    mut token_a_account,
                    _token_b_key,
                    _token_b_account,
                    _pool_key,
                    _pool_account,
                ) = accounts.setup_token_accounts(&user_key, &withdrawer_key, initial_a, initial_b, 0);

                let fee_a_amount = 2;
                let pool_fee_key = accounts.pool_fee_key;
                let mut pool_fee_account = accounts.pool_fee_account.clone();
                let fee_account = spl_token::state::Account::unpack(&pool_fee_account.data).unwrap();
                let pool_fee_amount = fee_account.amount;
                let swap_token_a =
                    spl_token::state::Account::unpack(&accounts.token_a_account.data).unwrap();

                let fee_account_owner = spl_token::state::Account::unpack(&pool_fee_account.data).unwrap().owner;
                let token_a_amount = swap_token_a.amount;
                accounts
                    .withdraw_single_token_type_exact_amount_out(
                        &fee_account_owner,
                        &pool_fee_key,
                        &mut pool_fee_account,
                        &token_a_key,
                        &mut token_a_account,
                        fee_a_amount,
                        pool_fee_amount,
                    ).unwrap();
                let swap_token_a =
                    spl_token::state::Account::unpack(&accounts.token_a_account.data).unwrap();

                assert_eq!(swap_token_a.amount, token_a_amount - fee_a_amount);
                let token_a = spl_token::state::Account::unpack(&token_a_account.data).unwrap();
                assert_eq!(token_a.amount, initial_a + fee_a_amount);
            }
    }

    fn check_valid_swap_curve(
        fees: Fees,
        curve_type: CurveType,
        calculator: Box<dyn CurveCalculator>,
        token_a_amount: u64,
        token_b_amount: u64,
    ) {
        let user_key = Pubkey::new_unique();
        let swapper_key = Pubkey::new_unique();

        let swap_curve = SwapCurve {
            curve_type,
            calculator,
        };

        let mut accounts = SwapAccountInfo::new(
            &user_key,
            fees.clone(),
            swap_curve.clone(),
            token_a_amount,
            token_b_amount,
        );
        let initial_a = token_a_amount / 5;
        let initial_b = token_b_amount / 5;
        accounts.initialize_swap().unwrap();

        let swap_token_a_key = accounts.token_a_key;
        let swap_token_b_key = accounts.token_b_key;

        let (
            token_a_key,
            mut token_a_account,
            token_b_key,
            mut token_b_account,
            _pool_key,
            _pool_account,
        ) = accounts.setup_token_accounts(&user_key, &swapper_key, initial_a, initial_b, 0);
        // swap one way
        let a_to_b_amount = initial_a / 10;
        let minimum_token_b_amount = 0;
        let pool_mint = spl_token::state::Mint::unpack(&accounts.pool_mint_account.data).unwrap();
        let initial_supply = pool_mint.supply;
        accounts
            .swap(
                &swapper_key,
                &token_a_key,
                &mut token_a_account,
                &swap_token_a_key,
                &swap_token_b_key,
                &token_b_key,
                &mut token_b_account,
                a_to_b_amount,
                minimum_token_b_amount,
            )
            .unwrap();

        let results = swap_curve
            .swap(
                a_to_b_amount.try_into().unwrap(),
                token_a_amount.try_into().unwrap(),
                token_b_amount.try_into().unwrap(),
                TradeDirection::AtoB,
                &fees,
            )
            .unwrap();

        let swap_token_a =
            spl_token::state::Account::unpack(&accounts.token_a_account.data).unwrap();
        let token_a_amount = swap_token_a.amount;
        assert_eq!(
            token_a_amount,
            TryInto::<u64>::try_into(results.new_swap_source_amount).unwrap()
        );
        let token_a = spl_token::state::Account::unpack(&token_a_account.data).unwrap();
        assert_eq!(token_a.amount, initial_a - a_to_b_amount);

        let swap_token_b =
            spl_token::state::Account::unpack(&accounts.token_b_account.data).unwrap();
        let token_b_amount = swap_token_b.amount;
        assert_eq!(
            token_b_amount,
            TryInto::<u64>::try_into(results.new_swap_destination_amount).unwrap()
        );
        let token_b = spl_token::state::Account::unpack(&token_b_account.data).unwrap();
        assert_eq!(
            token_b.amount,
            initial_b + to_u64(results.destination_amount_swapped).unwrap()
        );

        let first_fee = swap_curve
            .trading_tokens_to_pool_tokens(
                results.owner_fee,
                token_a_amount.try_into().unwrap(),
                token_b_amount.try_into().unwrap(),
                initial_supply.try_into().unwrap(),
                TradeDirection::AtoB,
                RoundDirection::Ceiling,
                &fees,
            )
            .unwrap();
        let fee_account =
            spl_token::state::Account::unpack(&accounts.pool_fee_account.data).unwrap();
        assert_eq!(
            fee_account.amount,
            TryInto::<u64>::try_into(first_fee).unwrap()
        );

        let first_swap_amount = results.destination_amount_swapped;

        // swap the other way
        let pool_mint = spl_token::state::Mint::unpack(&accounts.pool_mint_account.data).unwrap();
        let initial_supply = pool_mint.supply;

        let b_to_a_amount = initial_b / 10;
        let minimum_a_amount = 0;
        accounts
            .swap(
                &swapper_key,
                &token_b_key,
                &mut token_b_account,
                &swap_token_b_key,
                &swap_token_a_key,
                &token_a_key,
                &mut token_a_account,
                b_to_a_amount,
                minimum_a_amount,
            )
            .unwrap();

        let results = swap_curve
            .swap(
                b_to_a_amount.try_into().unwrap(),
                token_b_amount.try_into().unwrap(),
                token_a_amount.try_into().unwrap(),
                TradeDirection::BtoA,
                &fees,
            )
            .unwrap();

        let swap_token_a =
            spl_token::state::Account::unpack(&accounts.token_a_account.data).unwrap();
        let token_a_amount = swap_token_a.amount;
        assert_eq!(
            token_a_amount,
            TryInto::<u64>::try_into(results.new_swap_destination_amount).unwrap()
        );
        let token_a = spl_token::state::Account::unpack(&token_a_account.data).unwrap();
        assert_eq!(
            token_a.amount,
            initial_a - a_to_b_amount + to_u64(results.destination_amount_swapped).unwrap()
        );

        let swap_token_b =
            spl_token::state::Account::unpack(&accounts.token_b_account.data).unwrap();
        let token_b_amount = swap_token_b.amount;
        assert_eq!(
            token_b_amount,
            TryInto::<u64>::try_into(results.new_swap_source_amount).unwrap()
        );
        let token_b = spl_token::state::Account::unpack(&token_b_account.data).unwrap();
        assert_eq!(
            token_b.amount,
            initial_b + to_u64(first_swap_amount).unwrap()
                - to_u64(results.source_amount_swapped).unwrap()
        );

        let second_fee = swap_curve
            .trading_tokens_to_pool_tokens(
                results.owner_fee,
                token_a_amount.try_into().unwrap(),
                token_b_amount.try_into().unwrap(),
                initial_supply.try_into().unwrap(),
                TradeDirection::BtoA,
                RoundDirection::Ceiling,
                &fees,
            )
            .unwrap();
        let fee_account =
            spl_token::state::Account::unpack(&accounts.pool_fee_account.data).unwrap();
        assert_eq!(fee_account.amount, to_u64(first_fee + second_fee).unwrap());
    }

    #[test]
    fn test_valid_swap_curves_all_fees() {
        // All fees
        let trade_fee_numerator = 25;
        let trade_fee_denominator = 10000;
        let owner_trade_fee_numerator = 5;
        let owner_trade_fee_denominator = 10000;
        let owner_withdraw_fee_numerator = 0;
        let owner_withdraw_fee_denominator = 0;
        let host_fee_numerator = 0;
        let host_fee_denominator = 0;
        let fees = Fees {
            trade_fee_numerator,
            trade_fee_denominator,
            owner_trade_fee_numerator,
            owner_trade_fee_denominator,
            owner_withdraw_fee_numerator,
            owner_withdraw_fee_denominator,
            host_fee_numerator,
            host_fee_denominator,
        };

        let token_a_amount = 10_000_000_000;
        let token_b_amount = 50_000_000_000;

        check_valid_swap_curve(
            fees.clone(),
            CurveType::ConstantProduct,
            Box::new(ConstantProductCurve {}),
            token_a_amount,
            token_b_amount,
        );
    }

    #[test]
    fn test_valid_swap_curves_trade_fee_only() {
        let trade_fee_numerator = 25;
        let trade_fee_denominator = 10000;
        let owner_trade_fee_numerator = 5;
        let owner_trade_fee_denominator = 10000;
        let owner_withdraw_fee_numerator = 0;
        let owner_withdraw_fee_denominator = 0;
        let host_fee_numerator = 0;
        let host_fee_denominator = 0;
        let fees = Fees {
            trade_fee_numerator,
            trade_fee_denominator,
            owner_trade_fee_numerator,
            owner_trade_fee_denominator,
            owner_withdraw_fee_numerator,
            owner_withdraw_fee_denominator,
            host_fee_numerator,
            host_fee_denominator,
        };

        let token_a_amount = 10_000_000_000;
        let token_b_amount = 50_000_000_000;

        check_valid_swap_curve(
            fees.clone(),
            CurveType::ConstantProduct,
            Box::new(ConstantProductCurve {}),
            token_a_amount,
            token_b_amount,
        );
    }

    #[test]
    fn test_valid_swap_with_fee_constraints() {
        let mut owner_key = Pubkey::new_unique();

        if let Some(constraints) = crate::constraints::SWAP_CONSTRAINTS {
            owner_key = Pubkey::from_str(constraints.owner_key).expect("");
        }
        let trade_fee_numerator = 25;
        let trade_fee_denominator = 10000;
        let owner_trade_fee_numerator = 5;
        let owner_trade_fee_denominator = 10000;
        let owner_withdraw_fee_numerator = 0;
        let owner_withdraw_fee_denominator = 0;
        let host_fee_numerator = 0;
        let host_fee_denominator = 0;

        let token_a_amount = 1_000_000;
        let token_b_amount = 5_000_000;

        let fees = Fees {
            trade_fee_numerator,
            trade_fee_denominator,
            owner_trade_fee_numerator,
            owner_trade_fee_denominator,
            owner_withdraw_fee_numerator,
            owner_withdraw_fee_denominator,
            host_fee_numerator,
            host_fee_denominator,
        };

        let curve = ConstantProductCurve {};
        let swap_curve = SwapCurve {
            curve_type: CurveType::ConstantProduct,
            calculator: Box::new(curve),
        };

        let owner_key_str = &owner_key.to_string();
        let valid_curve_types = &[CurveType::ConstantProduct];
        let constraints = Some(SwapConstraints {
            owner_key: owner_key_str,
            valid_curve_types,
            fees: &fees,
        });
        let mut accounts = SwapAccountInfo::new(
            &owner_key,
            fees.clone(),
            swap_curve,
            token_a_amount,
            token_b_amount,
        );

        // initialize swap
        do_process_instruction_with_fee_constraints(
            initialize(
                &SWAP_PROGRAM_ID,
                &TOKEN_PROGRAM_ID,
                &accounts.swap_key,
                &accounts.authority_key,
                &accounts.token_a_key,
                &accounts.token_b_key,
                &accounts.pool_mint_key,
                &accounts.pool_fee_key,
                &accounts.pool_token_key,
                accounts.nonce,
                accounts.fees.clone(),
                accounts.swap_curve.clone(),
                &accounts.farming_state_key,
                &accounts.token_freeze_key,
            )
                .unwrap(),
            vec![
                &mut accounts.swap_account,
                &mut Account::default(),
                &mut accounts.token_a_account,
                &mut accounts.token_b_account,
                &mut accounts.pool_mint_account,
                &mut accounts.pool_fee_account,
                &mut accounts.pool_token_account,
                &mut Account::default(),
                &mut accounts.farming_state_account,
                &mut accounts.token_freeze_account,
            ],
            &constraints,
        )
            .unwrap();

        let authority_key = accounts.authority_key;

        let (
            token_a_key,
            mut token_a_account,
            token_b_key,
            mut token_b_account,
            pool_key,
            mut pool_account,
        ) = accounts.setup_token_accounts(
            &owner_key,
            &authority_key,
            token_a_amount,
            token_b_amount,
            0,
        );

        let amount_in = token_a_amount / 2;
        let minimum_amount_out = 0;

        // perform the swap
        do_process_instruction_with_fee_constraints(
            swap(
                &SWAP_PROGRAM_ID,
                &TOKEN_PROGRAM_ID,
                &accounts.swap_key,
                &accounts.authority_key,
                &accounts.authority_key,
                &token_a_key,
                &accounts.token_a_key,
                &accounts.token_b_key,
                &token_b_key,
                &accounts.pool_mint_key,
                &accounts.pool_fee_key,
                Some(&pool_key),
                Swap {
                    amount_in,
                    minimum_amount_out,
                },
            )
                .unwrap(),
            vec![
                &mut accounts.swap_account,
                &mut Account::default(),
                &mut Account::default(),
                &mut token_a_account,
                &mut accounts.token_a_account,
                &mut accounts.token_b_account,
                &mut token_b_account,
                &mut accounts.pool_mint_account,
                &mut accounts.pool_fee_account,
                &mut Account::default(),
                &mut pool_account,
            ],
            &constraints,
        )
            .unwrap();

        // check that fees were taken in the host fee account
        let host_fee_account = spl_token::state::Account::unpack(&pool_account.data).unwrap();
        let owner_fee_account =
            spl_token::state::Account::unpack(&accounts.pool_fee_account.data).unwrap();
        let total_fee = owner_fee_account.amount;
        assert_eq!(
            total_fee,
            host_fee_account.amount + owner_fee_account.amount
        );
    }

    #[test]
    fn test_invalid_swap() {
        let user_key = Pubkey::new_unique();
        let swapper_key = Pubkey::new_unique();
        let trade_fee_numerator = 25;
        let trade_fee_denominator = 10000;
        let owner_trade_fee_numerator = 5;
        let owner_trade_fee_denominator = 10000;
        let owner_withdraw_fee_numerator = 0;
        let owner_withdraw_fee_denominator = 0;
        let host_fee_numerator = 0;
        let host_fee_denominator = 0;
        let fees = Fees {
            trade_fee_numerator,
            trade_fee_denominator,
            owner_trade_fee_numerator,
            owner_trade_fee_denominator,
            owner_withdraw_fee_numerator,
            owner_withdraw_fee_denominator,
            host_fee_numerator,
            host_fee_denominator,
        };

        let token_a_amount = 1000;
        let token_b_amount = 5000;
        let curve_type = CurveType::ConstantProduct;
        let swap_curve = SwapCurve {
            curve_type,
            calculator: Box::new(ConstantProductCurve {}),
        };
        let mut accounts =
            SwapAccountInfo::new(&user_key, fees, swap_curve, token_a_amount, token_b_amount);

        let initial_a = token_a_amount / 5;
        let initial_b = token_b_amount / 5;
        let minimum_token_b_amount = initial_b / 2;

        let swap_token_a_key = accounts.token_a_key;
        let swap_token_b_key = accounts.token_b_key;

        // swap not initialized
        {
            let (
                token_a_key,
                mut token_a_account,
                token_b_key,
                mut token_b_account,
                _pool_key,
                _pool_account,
            ) = accounts.setup_token_accounts(&user_key, &swapper_key, initial_a, initial_b, 0);
            assert_eq!(
                Err(ProgramError::UninitializedAccount),
                accounts.swap(
                    &swapper_key,
                    &token_a_key,
                    &mut token_a_account,
                    &swap_token_a_key,
                    &swap_token_b_key,
                    &token_b_key,
                    &mut token_b_account,
                    initial_a,
                    minimum_token_b_amount,
                )
            );
        }

        accounts.initialize_swap().unwrap();

        // wrong swap account program id
        {
            let (
                token_a_key,
                mut token_a_account,
                token_b_key,
                mut token_b_account,
                _pool_key,
                _pool_account,
            ) = accounts.setup_token_accounts(&user_key, &swapper_key, initial_a, initial_b, 0);
            let old_swap_account = accounts.swap_account;
            let mut wrong_swap_account = old_swap_account.clone();
            wrong_swap_account.owner = TOKEN_PROGRAM_ID;
            accounts.swap_account = wrong_swap_account;
            assert_eq!(
                Err(ProgramError::IncorrectProgramId),
                accounts.swap(
                    &swapper_key,
                    &token_a_key,
                    &mut token_a_account,
                    &swap_token_a_key,
                    &swap_token_b_key,
                    &token_b_key,
                    &mut token_b_account,
                    initial_a,
                    minimum_token_b_amount,
                )
            );
            accounts.swap_account = old_swap_account;
        }

        // wrong nonce
        {
            let (
                token_a_key,
                mut token_a_account,
                token_b_key,
                mut token_b_account,
                _pool_key,
                _pool_account,
            ) = accounts.setup_token_accounts(&user_key, &swapper_key, initial_a, initial_b, 0);
            let old_authority = accounts.authority_key;
            let (bad_authority_key, _nonce) = Pubkey::find_program_address(
                &[&accounts.swap_key.to_bytes()[..]],
                &TOKEN_PROGRAM_ID,
            );
            accounts.authority_key = bad_authority_key;
            assert_eq!(
                Err(SwapError::InvalidProgramAddress.into()),
                accounts.swap(
                    &swapper_key,
                    &token_a_key,
                    &mut token_a_account,
                    &swap_token_a_key,
                    &swap_token_b_key,
                    &token_b_key,
                    &mut token_b_account,
                    initial_a,
                    minimum_token_b_amount,
                )
            );
            accounts.authority_key = old_authority;
        }

        // wrong token program id
        {
            let (
                token_a_key,
                mut token_a_account,
                token_b_key,
                mut token_b_account,
                _pool_key,
                _pool_account,
            ) = accounts.setup_token_accounts(&user_key, &swapper_key, initial_a, initial_b, 0);
            let wrong_program_id = Pubkey::new_unique();
            assert_eq!(
                Err(SwapError::IncorrectTokenProgramId.into()),
                do_process_instruction(
                    swap(
                        &SWAP_PROGRAM_ID,
                        &wrong_program_id,
                        &accounts.swap_key,
                        &accounts.authority_key,
                        &accounts.authority_key,
                        &token_a_key,
                        &accounts.token_a_key,
                        &accounts.token_b_key,
                        &token_b_key,
                        &accounts.pool_mint_key,
                        &accounts.pool_fee_key,
                        None,
                        Swap {
                            amount_in: initial_a,
                            minimum_amount_out: minimum_token_b_amount,
                        },
                    )
                        .unwrap(),
                    vec![
                        &mut accounts.swap_account,
                        &mut Account::default(),
                        &mut Account::default(),
                        &mut token_a_account,
                        &mut accounts.token_a_account,
                        &mut accounts.token_b_account,
                        &mut token_b_account,
                        &mut accounts.pool_mint_account,
                        &mut accounts.pool_fee_account,
                        &mut Account::default(),
                    ],
                ),
            );
        }

        // not enough token a to swap
        {
            let (
                token_a_key,
                mut token_a_account,
                token_b_key,
                mut token_b_account,
                _pool_key,
                _pool_account,
            ) = accounts.setup_token_accounts(&user_key, &swapper_key, initial_a, initial_b, 0);
            assert_eq!(
                Err(TokenError::InsufficientFunds.into()),
                accounts.swap(
                    &swapper_key,
                    &token_a_key,
                    &mut token_a_account,
                    &swap_token_a_key,
                    &swap_token_b_key,
                    &token_b_key,
                    &mut token_b_account,
                    initial_a * 2,
                    minimum_token_b_amount * 2,
                )
            );
        }

        // wrong swap token A / B accounts
        {
            let (
                token_a_key,
                mut token_a_account,
                token_b_key,
                mut token_b_account,
                _pool_key,
                _pool_account,
            ) = accounts.setup_token_accounts(&user_key, &swapper_key, initial_a, initial_b, 0);
            let user_transfer_key = Pubkey::new_unique();
            assert_eq!(
                Err(SwapError::IncorrectSwapAccount.into()),
                do_process_instruction(
                    swap(
                        &SWAP_PROGRAM_ID,
                        &TOKEN_PROGRAM_ID,
                        &accounts.swap_key,
                        &accounts.authority_key,
                        &user_transfer_key,
                        &token_a_key,
                        &token_a_key,
                        &token_b_key,
                        &token_b_key,
                        &accounts.pool_mint_key,
                        &accounts.pool_fee_key,
                        None,
                        Swap {
                            amount_in: initial_a,
                            minimum_amount_out: minimum_token_b_amount,
                        },
                    )
                        .unwrap(),
                    vec![
                        &mut accounts.swap_account,
                        &mut Account::default(),
                        &mut Account::default(),
                        &mut token_a_account.clone(),
                        &mut token_a_account,
                        &mut token_b_account.clone(),
                        &mut token_b_account,
                        &mut accounts.pool_mint_account,
                        &mut accounts.pool_fee_account,
                        &mut Account::default(),
                    ],
                ),
            );
        }

        // wrong user token A / B accounts
        {
            let (
                token_a_key,
                mut token_a_account,
                token_b_key,
                mut token_b_account,
                _pool_key,
                _pool_account,
            ) = accounts.setup_token_accounts(&user_key, &swapper_key, initial_a, initial_b, 0);
            assert_eq!(
                Err(TokenError::MintMismatch.into()),
                accounts.swap(
                    &swapper_key,
                    &token_b_key,
                    &mut token_b_account,
                    &swap_token_a_key,
                    &swap_token_b_key,
                    &token_a_key,
                    &mut token_a_account,
                    initial_a,
                    minimum_token_b_amount,
                )
            );
        }

        // swap from a to a
        {
            let (
                token_a_key,
                mut token_a_account,
                _token_b_key,
                _token_b_account,
                _pool_key,
                _pool_account,
            ) = accounts.setup_token_accounts(&user_key, &swapper_key, initial_a, initial_b, 0);
            assert_eq!(
                Err(SwapError::InvalidInput.into()),
                accounts.swap(
                    &swapper_key,
                    &token_a_key,
                    &mut token_a_account.clone(),
                    &swap_token_a_key,
                    &swap_token_a_key,
                    &token_a_key,
                    &mut token_a_account,
                    initial_a,
                    minimum_token_b_amount,
                )
            );
        }

        // incorrect mint provided
        {
            let (
                token_a_key,
                mut token_a_account,
                token_b_key,
                mut token_b_account,
                _pool_key,
                _pool_account,
            ) = accounts.setup_token_accounts(&user_key, &swapper_key, initial_a, initial_b, 0);
            let (pool_mint_key, pool_mint_account) =
                create_mint(&TOKEN_PROGRAM_ID, &accounts.authority_key, None);
            let old_pool_key = accounts.pool_mint_key;
            let old_pool_account = accounts.pool_mint_account;
            accounts.pool_mint_key = pool_mint_key;
            accounts.pool_mint_account = pool_mint_account;

            assert_eq!(
                Err(SwapError::IncorrectPoolMint.into()),
                accounts.swap(
                    &swapper_key,
                    &token_a_key,
                    &mut token_a_account,
                    &swap_token_a_key,
                    &swap_token_b_key,
                    &token_b_key,
                    &mut token_b_account,
                    initial_a,
                    minimum_token_b_amount,
                )
            );

            accounts.pool_mint_key = old_pool_key;
            accounts.pool_mint_account = old_pool_account;
        }

        // incorrect fee account provided
        {
            let (
                token_a_key,
                mut token_a_account,
                token_b_key,
                mut token_b_account,
                wrong_pool_key,
                wrong_pool_account,
            ) = accounts.setup_token_accounts(&user_key, &swapper_key, initial_a, initial_b, 0);
            let old_pool_fee_account = accounts.pool_fee_account;
            let old_pool_fee_key = accounts.pool_fee_key;
            accounts.pool_fee_account = wrong_pool_account;
            accounts.pool_fee_key = wrong_pool_key;
            assert_eq!(
                Err(SwapError::IncorrectFeeAccount.into()),
                accounts.swap(
                    &swapper_key,
                    &token_a_key,
                    &mut token_a_account,
                    &swap_token_a_key,
                    &swap_token_b_key,
                    &token_b_key,
                    &mut token_b_account,
                    initial_a,
                    minimum_token_b_amount,
                )
            );
            accounts.pool_fee_account = old_pool_fee_account;
            accounts.pool_fee_key = old_pool_fee_key;
        }

        // no approval
        {
            let (
                token_a_key,
                mut token_a_account,
                token_b_key,
                mut token_b_account,
                _pool_key,
                _pool_account,
            ) = accounts.setup_token_accounts(&user_key, &swapper_key, initial_a, initial_b, 0);
            let user_transfer_key = Pubkey::new_unique();
            assert_eq!(
                Err(TokenError::OwnerMismatch.into()),
                do_process_instruction(
                    swap(
                        &SWAP_PROGRAM_ID,
                        &TOKEN_PROGRAM_ID,
                        &accounts.swap_key,
                        &accounts.authority_key,
                        &user_transfer_key,
                        &token_a_key,
                        &accounts.token_a_key,
                        &accounts.token_b_key,
                        &token_b_key,
                        &accounts.pool_mint_key,
                        &accounts.pool_fee_key,
                        None,
                        Swap {
                            amount_in: initial_a,
                            minimum_amount_out: minimum_token_b_amount,
                        },
                    )
                        .unwrap(),
                    vec![
                        &mut accounts.swap_account,
                        &mut Account::default(),
                        &mut Account::default(),
                        &mut token_a_account,
                        &mut accounts.token_a_account,
                        &mut accounts.token_b_account,
                        &mut token_b_account,
                        &mut accounts.pool_mint_account,
                        &mut accounts.pool_fee_account,
                        &mut Account::default(),
                    ],
                ),
            );
        }

        // output token value 0
        {
            let (
                token_a_key,
                mut token_a_account,
                token_b_key,
                mut token_b_account,
                _pool_key,
                _pool_account,
            ) = accounts.setup_token_accounts(&user_key, &swapper_key, initial_a, initial_b, 0);
            assert_eq!(
                Err(SwapError::ZeroTradingTokens.into()),
                accounts.swap(
                    &swapper_key,
                    &token_b_key,
                    &mut token_b_account,
                    &swap_token_b_key,
                    &swap_token_a_key,
                    &token_a_key,
                    &mut token_a_account,
                    1,
                    1,
                )
            );
        }

        // slippage exceeded: minimum out amount too high
        {
            let (
                token_a_key,
                mut token_a_account,
                token_b_key,
                mut token_b_account,
                _pool_key,
                _pool_account,
            ) = accounts.setup_token_accounts(&user_key, &swapper_key, initial_a, initial_b, 0);
            assert_eq!(
                Err(SwapError::ExceededSlippage.into()),
                accounts.swap(
                    &swapper_key,
                    &token_a_key,
                    &mut token_a_account,
                    &swap_token_a_key,
                    &swap_token_b_key,
                    &token_b_key,
                    &mut token_b_account,
                    initial_a,
                    minimum_token_b_amount * 2,
                )
            );
        }

        // invalid input: can't use swap pool as user source / dest
        {
            let (
                token_a_key,
                mut token_a_account,
                token_b_key,
                mut token_b_account,
                _pool_key,
                _pool_account,
            ) = accounts.setup_token_accounts(&user_key, &swapper_key, initial_a, initial_b, 0);
            let mut swap_token_a_account = accounts.get_token_account(&swap_token_a_key).clone();
            let authority_key = accounts.authority_key;
            assert_eq!(
                Err(SwapError::InvalidInput.into()),
                accounts.swap(
                    &authority_key,
                    &swap_token_a_key,
                    &mut swap_token_a_account,
                    &swap_token_a_key,
                    &swap_token_b_key,
                    &token_b_key,
                    &mut token_b_account,
                    initial_a,
                    minimum_token_b_amount,
                )
            );
            let mut swap_token_b_account = accounts.get_token_account(&swap_token_b_key).clone();
            assert_eq!(
                Err(SwapError::InvalidInput.into()),
                accounts.swap(
                    &swapper_key,
                    &token_a_key,
                    &mut token_a_account,
                    &swap_token_a_key,
                    &swap_token_b_key,
                    &swap_token_b_key,
                    &mut swap_token_b_account,
                    initial_a,
                    minimum_token_b_amount,
                )
            );
        }

        // still correct: constraint specified, no host fee account
        {
            let authority_key = accounts.authority_key;
            let (
                token_a_key,
                mut token_a_account,
                token_b_key,
                mut token_b_account,
                _pool_key,
                _pool_account,
            ) = accounts.setup_token_accounts(&user_key, &authority_key, initial_a, initial_b, 0);
            let owner_key = &swapper_key.to_string();
            let fees = Fees {
                trade_fee_numerator,
                trade_fee_denominator,
                owner_trade_fee_numerator,
                owner_trade_fee_denominator,
                owner_withdraw_fee_numerator,
                owner_withdraw_fee_denominator,
                host_fee_numerator,
                host_fee_denominator,
            };
            let constraints = Some(SwapConstraints {
                owner_key,
                valid_curve_types: &[],
                fees: &fees,
            });
            do_process_instruction_with_fee_constraints(
                swap(
                    &SWAP_PROGRAM_ID,
                    &TOKEN_PROGRAM_ID,
                    &accounts.swap_key,
                    &accounts.authority_key,
                    &accounts.authority_key,
                    &token_a_key,
                    &accounts.token_a_key,
                    &accounts.token_b_key,
                    &token_b_key,
                    &accounts.pool_mint_key,
                    &accounts.pool_fee_key,
                    None,
                    Swap {
                        amount_in: initial_a,
                        minimum_amount_out: minimum_token_b_amount,
                    },
                )
                    .unwrap(),
                vec![
                    &mut accounts.swap_account,
                    &mut Account::default(),
                    &mut Account::default(),
                    &mut token_a_account,
                    &mut accounts.token_a_account,
                    &mut accounts.token_b_account,
                    &mut token_b_account,
                    &mut accounts.pool_mint_account,
                    &mut accounts.pool_fee_account,
                    &mut Account::default(),
                ],
                &constraints,
            )
                .unwrap();
        }
    }

    #[test]
    fn test_init_farming() {
        let trade_fee_numerator = 25;
        let trade_fee_denominator = 10000;
        let owner_trade_fee_numerator = 5;
        let owner_trade_fee_denominator = 10000;
        let owner_withdraw_fee_numerator = 0;
        let owner_withdraw_fee_denominator = 0;
        let host_fee_numerator = 0;
        let host_fee_denominator = 0;

        let token_a_amount = 1_000_000_000;
        let token_b_amount = 10;
        let fees = Fees {
            trade_fee_numerator,
            trade_fee_denominator,
            owner_trade_fee_numerator,
            owner_trade_fee_denominator,
            owner_withdraw_fee_numerator,
            owner_withdraw_fee_denominator,
            host_fee_numerator,
            host_fee_denominator,
        };

        let curve_type = CurveType::ConstantProduct;
        let swap_curve = SwapCurve {
            curve_type,
            calculator: Box::new(ConstantProductCurve {}),
        };
        let user_key = Pubkey::new_unique();

        let token_amount = 100_000;
        let tokens_per_period = 100;
        let period_length = 1;

        let clock_key = Pubkey::from_str("SysvarC1ock11111111111111111111111111111111")
            .expect("Clock pubkey creation failed");
        let mut clock = Clock::default();
        clock.unix_timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH).expect("wrong current system time")
            .as_secs() as i64;
        let mut clock_account = Account::new_data(
            1000000000,
            &clock,
            &solana_program::system_program::ID,
        ).expect("account creation failed");


        let mut accounts =
            SwapAccountInfo::new(&user_key, fees, swap_curve, token_a_amount, token_b_amount);

        accounts.initialize_swap().unwrap();

        let mut state = FarmingStateInfo::new(
            100000,
            user_key,
            accounts.authority_key);

        accounts.init_farming(
            &user_key,
            &clock_key,
            &mut clock_account,
            &state.owner_farming_token_key,
            &mut state.owner_farming_token_account,
            &state.swap_farming_token_key,
            &mut state.swap_farming_token_account,
            token_amount,
            tokens_per_period,
            period_length,
        ).unwrap();

        let swap_farming_token =
            spl_token::state::Account::unpack(&state.swap_farming_token_account.data).unwrap();
        assert_eq!(swap_farming_token.amount, token_amount);
        let farming_state = FarmingState::unpack(&accounts.farming_state_account.data).unwrap();
        assert_eq!(farming_state.tokens_per_period, tokens_per_period);
        assert_eq!(farming_state.period_length, period_length);
    }

    fn create_user_and_deposit(
        accounts: &mut SwapAccountInfo,
        state: &mut FarmingStateInfo,
        user_key: &Pubkey,
        token_a_amount: u64,
        token_b_amount: u64,
        pool_token_amount: u64,
    ) -> UserFarmingInfo {
        let mut user_info = UserFarmingInfo::new(
            *user_key,
            state.farming_mint_key,
            &mut state.farming_mint_account,
            accounts.pool_mint_key,
            &mut accounts.pool_mint_account,
            accounts.token_a_mint_key,
            &mut accounts.token_a_mint_account,
            accounts.token_b_mint_key,
            &mut accounts.token_b_mint_account,
            token_a_amount,
            token_b_amount,
        );

        accounts.deposit_all_user_token_types(
            &mut user_info,
            pool_token_amount,
            token_a_amount,
            token_b_amount,
        ).unwrap();

        user_info
    }

    #[test]
    fn test_start_farming() {
        let trade_fee_numerator = 25;
        let trade_fee_denominator = 10000;
        let owner_trade_fee_numerator = 5;
        let owner_trade_fee_denominator = 10000;
        let owner_withdraw_fee_numerator = 0;
        let owner_withdraw_fee_denominator = 0;
        let host_fee_numerator = 0;
        let host_fee_denominator = 0;

        let token_a_amount = 1_000_000_000;
        let token_b_amount = 10;
        let fees = Fees {
            trade_fee_numerator,
            trade_fee_denominator,
            owner_trade_fee_numerator,
            owner_trade_fee_denominator,
            owner_withdraw_fee_numerator,
            owner_withdraw_fee_denominator,
            host_fee_numerator,
            host_fee_denominator,
        };

        let curve_type = CurveType::ConstantProduct;
        let swap_curve = SwapCurve {
            curve_type,
            calculator: Box::new(ConstantProductCurve {}),
        };
        let user_key = Pubkey::new_unique();

        let token_amount = 100_000;
        let tokens_per_period = 100;
        let period_length = 1;
        let tokens_to_freeze = 100_000;

        let pool_amount = INITIAL_SWAP_POOL_AMOUNT / 10;
        let deposit_a = token_a_amount / 10;
        let deposit_b = token_b_amount / 10;

        let clock_key = Pubkey::from_str("SysvarC1ock11111111111111111111111111111111")
            .expect("Clock pubkey creation failed");
        let mut clock = Clock::default();
        clock.unix_timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH).expect("wrong current system time")
            .as_secs() as i64;
        let mut clock_account = Account::new_data(
            1000000000,
            &clock,
            &solana_program::system_program::ID,
        ).expect("account creation failed");

        let mut accounts =
            SwapAccountInfo::new(&user_key, fees, swap_curve, token_a_amount, token_b_amount);

        accounts.initialize_swap().unwrap();

        let mut state = FarmingStateInfo::new(
            token_amount,
            user_key,
            accounts.authority_key);

        let mut user_one = create_user_and_deposit(
            &mut accounts,
            &mut state,
            &user_key,
            deposit_a,
            deposit_b,
            pool_amount.try_into().unwrap(),
        );
        let mut user_two = create_user_and_deposit(
            &mut accounts,
            &mut state,
            &user_key,
            deposit_a,
            deposit_b,
            pool_amount.try_into().unwrap(),
        );

        accounts.init_farming(
            &user_key,
            &clock_key,
            &mut clock_account,
            &state.owner_farming_token_key,
            &mut state.owner_farming_token_account,
            &state.swap_farming_token_key,
            &mut state.swap_farming_token_account,
            token_amount,
            tokens_per_period,
            period_length,
        ).unwrap();

        accounts.start_farming(
            &user_one.user_key,
            &user_one.pool_key,
            &mut user_one.pool_account,
            &user_one.farming_ticket_key,
            &mut user_one.farming_ticket_account,
            &clock_key,
            &mut clock_account,
            tokens_to_freeze,
        ).unwrap();

        accounts.start_farming(
            &user_two.user_key,
            &user_two.pool_key,
            &mut user_two.pool_account,
            &user_two.farming_ticket_key,
            &mut user_two.farming_ticket_account,
            &clock_key,
            &mut clock_account,
            tokens_to_freeze,
        ).unwrap();

        let swap_token_freeze =
            spl_token::state::Account::unpack(&accounts.token_freeze_account.data).unwrap();
        assert_eq!(swap_token_freeze.amount, tokens_to_freeze * 2);
        let farming_ticket = FarmingTicket::unpack(&user_one.farming_ticket_account.data).unwrap();
        assert_eq!(farming_ticket.tokens_frozen, tokens_to_freeze);
        assert_eq!(farming_ticket.start_time, clock.unix_timestamp);
        assert_eq!(farming_ticket.token_authority, user_one.user_key);
        assert_eq!(farming_ticket.farming_state, accounts.farming_state_key);
    }

    #[test]
    fn test_take_farming_snapshot() {
        let trade_fee_numerator = 25;
        let trade_fee_denominator = 10000;
        let owner_trade_fee_numerator = 5;
        let owner_trade_fee_denominator = 10000;
        let owner_withdraw_fee_numerator = 0;
        let owner_withdraw_fee_denominator = 0;
        let host_fee_numerator = 0;
        let host_fee_denominator = 0;

        let token_a_amount = 1_000_000_000;
        let token_b_amount = 10;
        let fees = Fees {
            trade_fee_numerator,
            trade_fee_denominator,
            owner_trade_fee_numerator,
            owner_trade_fee_denominator,
            owner_withdraw_fee_numerator,
            owner_withdraw_fee_denominator,
            host_fee_numerator,
            host_fee_denominator,
        };

        let curve_type = CurveType::ConstantProduct;
        let swap_curve = SwapCurve {
            curve_type,
            calculator: Box::new(ConstantProductCurve {}),
        };
        let user_key = Pubkey::new_unique();

        let token_amount = 100_000;
        let tokens_per_period = 100;
        let period_length = 1;
        let tokens_to_freeze_one = 100_000;
        let tokens_to_freeze_two = 1_000_000;
        let time_period_one = 100;

        let pool_amount = INITIAL_SWAP_POOL_AMOUNT / 10;
        let deposit_a = token_a_amount / 10;
        let deposit_b = token_b_amount / 10;

        let clock_key = Pubkey::from_str("SysvarC1ock11111111111111111111111111111111")
            .expect("Clock pubkey creation failed");
        let mut current_timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH).expect("wrong current system time")
            .as_secs() as i64;
        let mut clock_account = get_clock_for_time(current_timestamp);

        let mut accounts =
            SwapAccountInfo::new(&user_key, fees, swap_curve, token_a_amount, token_b_amount);

        accounts.initialize_swap().unwrap();

        let mut state = FarmingStateInfo::new(
            100000,
            user_key,
            accounts.authority_key);

        let mut user_one = create_user_and_deposit(
            &mut accounts,
            &mut state,
            &user_key,
            deposit_a,
            deposit_b,
            pool_amount.try_into().unwrap(),
        );
        let mut user_two = create_user_and_deposit(
            &mut accounts,
            &mut state,
            &user_key,
            deposit_a,
            deposit_b,
            pool_amount.try_into().unwrap(),
        );

        accounts.init_farming(
            &user_key,
            &clock_key,
            &mut clock_account,
            &state.owner_farming_token_key,
            &mut state.owner_farming_token_account,
            &state.swap_farming_token_key,
            &mut state.swap_farming_token_account,
            token_amount,
            tokens_per_period,
            period_length,
        ).unwrap();

        accounts.start_farming(
            &user_one.user_key,
            &user_one.pool_key,
            &mut user_one.pool_account,
            &user_one.farming_ticket_key,
            &mut user_one.farming_ticket_account,
            &clock_key,
            &mut clock_account,
            tokens_to_freeze_one,
        ).unwrap();

        accounts.start_farming(
            &user_two.user_key,
            &user_two.pool_key,
            &mut user_two.pool_account,
            &user_two.farming_ticket_key,
            &mut user_two.farming_ticket_account,
            &clock_key,
            &mut clock_account,
            tokens_to_freeze_two,
        ).unwrap();

        current_timestamp += time_period_one;
        clock_account = get_clock_for_time(current_timestamp);

        accounts.take_farming_snapshot(
            &clock_key,
            &mut clock_account,
        ).unwrap();

        let swap_token_freeze =
            spl_token::state::Account::unpack(&accounts.token_freeze_account.data).unwrap();
        assert_eq!(swap_token_freeze.amount, tokens_to_freeze_one + tokens_to_freeze_two);
        let farming_state = FarmingState::unpack(&accounts.farming_state_account.data).unwrap();
        assert_eq!(farming_state.farming_snapshots.next_index, 1);
        let last_snapshot = farming_state
            .farming_snapshots
            .snapshots.as_slice()[0];
        assert_eq!(last_snapshot.tokens_frozen, tokens_to_freeze_one + tokens_to_freeze_two);
        assert_eq!(last_snapshot.farming_tokens,
                   tokens_per_period * time_period_one as u64);
        assert_eq!(last_snapshot.time, current_timestamp);
    }

    fn check_diff_within_error_range(
        expected_value: u64,
        given_value: u64,
        percentage_numerator: u128,
        percentage_denominator: u128,
    ) -> bool {
        let diff = i64::abs(expected_value as i64 - given_value as i64) as u128;
        diff <= (expected_value as u128 * percentage_numerator) / percentage_denominator
    }

    #[test]
    fn test_withdraw_farmed() {
        let trade_fee_numerator = 25;
        let trade_fee_denominator = 10000;
        let owner_trade_fee_numerator = 5;
        let owner_trade_fee_denominator = 10000;
        let owner_withdraw_fee_numerator = 0;
        let owner_withdraw_fee_denominator = 0;
        let host_fee_numerator = 0;
        let host_fee_denominator = 0;

        let token_a_amount = 1_000_000_000;
        let token_b_amount = 10;
        let fees = Fees {
            trade_fee_numerator,
            trade_fee_denominator,
            owner_trade_fee_numerator,
            owner_trade_fee_denominator,
            owner_withdraw_fee_numerator,
            owner_withdraw_fee_denominator,
            host_fee_numerator,
            host_fee_denominator,
        };

        let curve_type = CurveType::ConstantProduct;
        let swap_curve = SwapCurve {
            curve_type,
            calculator: Box::new(ConstantProductCurve {}),
        };
        let user_key = Pubkey::new_unique();

        let token_amount = 100_000_000;
        let tokens_per_period = 100;
        let period_length: u64 = 60 * 60 * 24;
        let tokens_to_freeze_one = 100_000;
        let tokens_to_freeze_two = 1_000_000;

        let pool_amount = INITIAL_SWAP_POOL_AMOUNT / 10;
        let deposit_a = token_a_amount / 10;
        let deposit_b = token_b_amount / 10;
        let time_period_one = period_length as i64;
        let period_one_snapshots = 15;
        let time_period_two = (period_length * 30) as i64;
        let period_two_snapshots = 3;
        let time_period_three = crate::yield_farming::farming_state::NO_WITHDRAWAL_TIME;
        let time_period_four = (period_length * 25) as i64;

        let clock_key = Pubkey::from_str("SysvarC1ock11111111111111111111111111111111")
            .expect("Clock pubkey creation failed");

        let mut current_timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH).expect("wrong current system time")
            .as_secs() as i64;
        let mut clock_account = get_clock_for_time(current_timestamp);

        let mut accounts =
            SwapAccountInfo::new(&user_key, fees, swap_curve, token_a_amount, token_b_amount);

        accounts.initialize_swap().unwrap();

        let mut state = FarmingStateInfo::new(
            token_amount,
            user_key,
            accounts.authority_key);

        let mut user_one = create_user_and_deposit(
            &mut accounts,
            &mut state,
            &user_key,
            deposit_a,
            deposit_b,
            pool_amount.try_into().unwrap(),
        );
        let mut user_two = create_user_and_deposit(
            &mut accounts,
            &mut state,
            &user_key,
            deposit_a,
            deposit_b,
            pool_amount.try_into().unwrap(),
        );

        accounts.init_farming(
            &user_key,
            &clock_key,
            &mut clock_account,
            &state.owner_farming_token_key,
            &mut state.owner_farming_token_account,
            &state.swap_farming_token_key,
            &mut state.swap_farming_token_account,
            token_amount,
            tokens_per_period,
            period_length,
        ).unwrap();

        accounts.start_farming(
            &user_one.user_key,
            &user_one.pool_key,
            &mut user_one.pool_account,
            &user_one.farming_ticket_key,
            &mut user_one.farming_ticket_account,
            &clock_key,
            &mut clock_account,
            tokens_to_freeze_one,
        ).unwrap();

        accounts.start_farming(
            &user_two.user_key,
            &user_two.pool_key,
            &mut user_two.pool_account,
            &user_two.farming_ticket_key,
            &mut user_two.farming_ticket_account,
            &clock_key,
            &mut clock_account,
            tokens_to_freeze_two,
        ).unwrap();

        let mut i = 0;
        while i < period_one_snapshots {
            current_timestamp += time_period_one;
            clock_account = get_clock_for_time(current_timestamp);

            accounts.take_farming_snapshot(
                &clock_key,
                &mut clock_account,
            ).unwrap();
            i += 1;
        }
        assert_eq!(Err(FarmingError::MinimumWithdrawalTimeNotPassed.into()),
                   accounts.withdraw_farmed(
                       &user_one.user_farming_token_key,
                       &mut user_one.user_farming_token_account,
                       &user_one.farming_ticket_key,
                       &mut user_one.farming_ticket_account,
                       &state.swap_farming_token_key,
                       &mut state.swap_farming_token_account,
                       &user_one.user_key,
                       &clock_key,
                       &mut clock_account,
                   )
        );

        let mut i = 0;
        while i < period_two_snapshots {
            current_timestamp += time_period_two;
            clock_account = get_clock_for_time(current_timestamp);

            accounts.take_farming_snapshot(
                &clock_key,
                &mut clock_account,
            ).unwrap();
            i += 1;
        }

        assert_eq!(Err(FarmingError::MinimumWithdrawalTimeNotPassed.into()),
                   accounts.withdraw_farmed(
                       &user_two.user_farming_token_key,
                       &mut user_two.user_farming_token_account,
                       &user_two.farming_ticket_key,
                       &mut user_two.farming_ticket_account,
                       &state.swap_farming_token_key,
                       &mut state.swap_farming_token_account,
                       &user_two.user_key,
                       &clock_key,
                       &mut clock_account,
                   ));

        current_timestamp += time_period_three;
        clock_account = get_clock_for_time(current_timestamp);
        let user_one_withdrawal_timestamp = current_timestamp;

        accounts.take_farming_snapshot(
            &clock_key,
            &mut clock_account,
        ).unwrap();
        accounts.withdraw_farmed(
            &user_one.user_farming_token_key,
            &mut user_one.user_farming_token_account,
            &user_one.farming_ticket_key,
            &mut user_one.farming_ticket_account,
            &state.swap_farming_token_key,
            &mut state.swap_farming_token_account,
            &user_one.user_key,
            &clock_key,
            &mut clock_account,
        ).unwrap();

        current_timestamp += time_period_four;
        clock_account = get_clock_for_time(current_timestamp);
        accounts.take_farming_snapshot(
            &clock_key,
            &mut clock_account,
        ).unwrap();

        accounts.withdraw_farmed(
            &user_two.user_farming_token_key,
            &mut user_two.user_farming_token_account,
            &user_two.farming_ticket_key,
            &mut user_two.farming_ticket_account,
            &state.swap_farming_token_key,
            &mut state.swap_farming_token_account,
            &user_two.user_key,
            &clock_key,
            &mut clock_account,
        ).unwrap();

        let farming_ticket_one = FarmingTicket::unpack(&user_one.farming_ticket_account.data).unwrap();
        let farming_ticket_two = FarmingTicket::unpack(&user_two.farming_ticket_account.data).unwrap();

        let swap_token_freeze =
            spl_token::state::Account::unpack(&accounts.token_freeze_account.data).unwrap();
        assert_eq!(swap_token_freeze.amount, tokens_to_freeze_one + tokens_to_freeze_two);

        let farmed_tokens_one =
            tokens_per_period
                * ((period_one_snapshots +
                time_period_two / (period_length as i64) * period_two_snapshots +
                time_period_three / (period_length as i64)) as u64);
        let farmed_tokens_two = farmed_tokens_one + tokens_per_period * (time_period_four as u64 / period_length);
        let farmed_tokens_one = ((farmed_tokens_one as u128 * tokens_to_freeze_one as u128)
            / swap_token_freeze.amount as u128) as u64;
        let farmed_tokens_two = ((farmed_tokens_two as u128 * tokens_to_freeze_two as u128)
            / swap_token_freeze.amount as u128) as u64;

        let user_one_farming_token =
            spl_token::state::Account::unpack(&user_one.user_farming_token_account.data).unwrap();
        let user_two_farming_token =
            spl_token::state::Account::unpack(&user_two.user_farming_token_account.data).unwrap();

        let swap_farming_token =
            spl_token::state::Account::unpack(&state.swap_farming_token_account.data).unwrap();
        assert_eq!(check_diff_within_error_range(swap_farming_token.amount,
                                                 token_amount - farmed_tokens_one - farmed_tokens_two,
                                                 1,
                                                 100),
                   true);

        assert_eq!(check_diff_within_error_range(user_one_farming_token.amount,
                                                 farmed_tokens_one,
                                                 1,
                                                 100),
                   true);
        assert_eq!(check_diff_within_error_range(user_two_farming_token.amount,
                                                 farmed_tokens_two,
                                                 1,
                                                 100),
                   true);
        assert_eq!(farming_ticket_one.start_time, user_one_withdrawal_timestamp);
        assert_eq!(farming_ticket_two.start_time, current_timestamp);
    }

    #[test]
    fn test_end_farming() {
        let trade_fee_numerator = 25;
        let trade_fee_denominator = 10000;
        let owner_trade_fee_numerator = 5;
        let owner_trade_fee_denominator = 10000;
        let owner_withdraw_fee_numerator = 0;
        let owner_withdraw_fee_denominator = 0;
        let host_fee_numerator = 0;
        let host_fee_denominator = 0;

        let token_a_amount = 1_000_000_000;
        let token_b_amount = 10;
        let fees = Fees {
            trade_fee_numerator,
            trade_fee_denominator,
            owner_trade_fee_numerator,
            owner_trade_fee_denominator,
            owner_withdraw_fee_numerator,
            owner_withdraw_fee_denominator,
            host_fee_numerator,
            host_fee_denominator,
        };

        let curve_type = CurveType::ConstantProduct;
        let swap_curve = SwapCurve {
            curve_type,
            calculator: Box::new(ConstantProductCurve {}),
        };
        let user_key = Pubkey::new_unique();

        let token_amount = 100_000_000;
        let tokens_per_period = 100;
        let period_length: u64 = 60 * 60 * 24;
        let tokens_to_freeze_one = 100_000;
        let tokens_to_freeze_two = 1_000_000;

        let pool_amount = INITIAL_SWAP_POOL_AMOUNT / 10;
        let deposit_a = token_a_amount / 10;
        let deposit_b = token_b_amount / 10;
        let time_period_one = period_length as i64;
        let period_one_snapshots = 15;
        let time_period_two = (period_length * 30) as i64;
        let period_two_snapshots = 3;
        let time_period_three = crate::yield_farming::farming_state::NO_WITHDRAWAL_TIME;

        let clock_key = Pubkey::from_str("SysvarC1ock11111111111111111111111111111111")
            .expect("Clock pubkey creation failed");

        let mut current_timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH).expect("wrong current system time")
            .as_secs() as i64;
        let mut clock_account = get_clock_for_time(current_timestamp);

        let mut accounts =
            SwapAccountInfo::new(&user_key, fees, swap_curve, token_a_amount, token_b_amount);

        accounts.initialize_swap().unwrap();

        let mut state = FarmingStateInfo::new(
            token_amount,
            user_key,
            accounts.authority_key);

        let mut user_one = create_user_and_deposit(
            &mut accounts,
            &mut state,
            &user_key,
            deposit_a,
            deposit_b,
            pool_amount.try_into().unwrap(),
        );
        let mut user_two = create_user_and_deposit(
            &mut accounts,
            &mut state,
            &user_key,
            deposit_a,
            deposit_b,
            pool_amount.try_into().unwrap(),
        );

        accounts.init_farming(
            &user_key,
            &clock_key,
            &mut clock_account,
            &state.owner_farming_token_key,
            &mut state.owner_farming_token_account,
            &state.swap_farming_token_key,
            &mut state.swap_farming_token_account,
            token_amount,
            tokens_per_period,
            period_length,
        ).unwrap();

        accounts.start_farming(
            &user_one.user_key,
            &user_one.pool_key,
            &mut user_one.pool_account,
            &user_one.farming_ticket_key,
            &mut user_one.farming_ticket_account,
            &clock_key,
            &mut clock_account,
            tokens_to_freeze_one,
        ).unwrap();

        accounts.start_farming(
            &user_two.user_key,
            &user_two.pool_key,
            &mut user_two.pool_account,
            &user_two.farming_ticket_key,
            &mut user_two.farming_ticket_account,
            &clock_key,
            &mut clock_account,
            tokens_to_freeze_two,
        ).unwrap();

        let mut i = 0;
        while i < period_one_snapshots {
            current_timestamp += time_period_one;
            clock_account = get_clock_for_time(current_timestamp);

            accounts.take_farming_snapshot(
                &clock_key,
                &mut clock_account,
            ).unwrap();
            i += 1;
        }
        assert_eq!(Err(FarmingError::MinimumWithdrawalTimeNotPassed.into()),
                   accounts.withdraw_farmed(
                       &user_one.user_farming_token_key,
                       &mut user_one.user_farming_token_account,
                       &user_one.farming_ticket_key,
                       &mut user_one.farming_ticket_account,
                       &state.swap_farming_token_key,
                       &mut state.swap_farming_token_account,
                       &user_one.user_key,
                       &clock_key,
                       &mut clock_account,
                   )
        );

        let mut i = 0;
        while i < period_two_snapshots {
            current_timestamp += time_period_two;
            clock_account = get_clock_for_time(current_timestamp);

            accounts.take_farming_snapshot(
                &clock_key,
                &mut clock_account,
            ).unwrap();
            i += 1;
        }
        println!("current stamp {}", current_timestamp);

        current_timestamp += 1;
        clock_account = get_clock_for_time(current_timestamp);

        accounts.end_farming(
            &user_one.farming_ticket_key,
            &mut user_one.farming_ticket_account,
            &user_one.pool_key,
            &mut user_one.pool_account,
            &user_one.user_key,
            &clock_key,
            &mut clock_account,
        ).unwrap();

        assert_eq!(Err(FarmingError::MinimumWithdrawalTimeNotPassed.into()),
                   accounts.withdraw_farmed(
                       &user_two.user_farming_token_key,
                       &mut user_two.user_farming_token_account,
                       &user_two.farming_ticket_key,
                       &mut user_two.farming_ticket_account,
                       &state.swap_farming_token_key,
                       &mut state.swap_farming_token_account,
                       &user_two.user_key,
                       &clock_key,
                       &mut clock_account,
                   ));

        current_timestamp += time_period_three;
        clock_account = get_clock_for_time(current_timestamp);

        accounts.take_farming_snapshot(
            &clock_key,
            &mut clock_account,
        ).unwrap();

        accounts.withdraw_farmed(
            &user_one.user_farming_token_key,
            &mut user_one.user_farming_token_account,
            &user_one.farming_ticket_key,
            &mut user_one.farming_ticket_account,
            &state.swap_farming_token_key,
            &mut state.swap_farming_token_account,
            &user_one.user_key,
            &clock_key,
            &mut clock_account,
        ).unwrap();

        assert_eq!(Err(ProgramError::InvalidAccountData),
                   accounts.withdraw_farmed(
                       &user_one.user_farming_token_key,
                       &mut user_one.user_farming_token_account,
                       &user_one.farming_ticket_key,
                       &mut user_one.farming_ticket_account,
                       &state.swap_farming_token_key,
                       &mut state.swap_farming_token_account,
                       &user_one.user_key,
                       &clock_key,
                       &mut clock_account,
                   ));

        accounts.withdraw_farmed(
            &user_two.user_farming_token_key,
            &mut user_two.user_farming_token_account,
            &user_two.farming_ticket_key,
            &mut user_two.farming_ticket_account,
            &state.swap_farming_token_key,
            &mut state.swap_farming_token_account,
            &user_two.user_key,
            &clock_key,
            &mut clock_account,
        ).unwrap();

        //let farming_state = FarmingState::unpack(&accounts.farming_state_account.data).unwrap();
        let swap_token_freeze =
            spl_token::state::Account::unpack(&accounts.token_freeze_account.data).unwrap();
        assert_eq!(swap_token_freeze.amount, tokens_to_freeze_two);
        let period_one_tokens = tokens_per_period * period_one_snapshots;
        let period_two_tokens = (tokens_per_period
            * period_two_snapshots * time_period_two as u64) / period_length;
        let period_three_tokens = (tokens_per_period
            * time_period_three as u64) / period_length;

        println!("{} {} {}",  period_one_tokens ,period_two_tokens, period_three_tokens);
        let farmed_tokens_one = period_one_tokens + period_two_tokens;
        let farmed_tokens_two = period_one_tokens + period_two_tokens;
        let farmed_tokens_one = ((farmed_tokens_one as u128 * tokens_to_freeze_one as u128)
            / ((tokens_to_freeze_one + tokens_to_freeze_two) as u128)) as u64;
        let farmed_tokens_two = ((farmed_tokens_two as u128 * tokens_to_freeze_two as u128)
            / ((tokens_to_freeze_one + tokens_to_freeze_two) as u128)) as u64
            + period_three_tokens;
        let farmed_tokens_total = farmed_tokens_one + farmed_tokens_two;

        let swap_farming_token =
            spl_token::state::Account::unpack(&state.swap_farming_token_account.data).unwrap();

        assert_eq!(check_diff_within_error_range(
            swap_farming_token.amount,
            token_amount - farmed_tokens_total,
            1,
            100,
        ), true);
        let user_one_farming_token =
            spl_token::state::Account::unpack(&user_one.user_farming_token_account.data).unwrap();
        let user_two_farming_token =
            spl_token::state::Account::unpack(&user_two.user_farming_token_account.data).unwrap();

        assert_eq!(check_diff_within_error_range(
            user_one_farming_token.amount,
            farmed_tokens_one,
            1,
            100), true);
        assert_eq!(check_diff_within_error_range(
            user_two_farming_token.amount,
            farmed_tokens_two,
            1,
            100), true);
        assert_eq!(FarmingTicket::is_initialized(user_one.farming_ticket_account.data.as_slice()), false);
    }
/*
    #[test]
    fn test_custom_util() {
        let user_key = Pubkey::new_unique();
        let trade_fee_numerator = 25;
        let trade_fee_denominator = 10000;
        let owner_trade_fee_numerator = 5;
        let owner_trade_fee_denominator = 10000;
        let owner_withdraw_fee_numerator = 0;
        let owner_withdraw_fee_denominator = 0;
        let host_fee_numerator = 0;
        let host_fee_denominator = 0;

        let fees = Fees {
            trade_fee_numerator,
            trade_fee_denominator,
            owner_trade_fee_numerator,
            owner_trade_fee_denominator,
            owner_withdraw_fee_numerator,
            owner_withdraw_fee_denominator,
            host_fee_numerator,
            host_fee_denominator,
        };

        let token_a_amount = 1000000;
        let token_b_amount = 2000000;
        let curve_type = CurveType::ConstantProduct;
        let swap_curve = SwapCurve {
            curve_type,
            calculator: Box::new(ConstantProductCurve {}),
        };

        let withdrawer_key = Pubkey::new_unique();
        let initial_a = token_a_amount / 10;
        let initial_b = token_b_amount / 10;
        let initial_pool = swap_curve.calculator.new_pool_supply() / 10;
        let withdraw_amount = initial_pool / 4;
        let minimum_token_a_amount = initial_a / 40;
        let minimum_token_b_amount = initial_b / 40;

        let mut accounts =
            SwapAccountInfo::new(&user_key, fees, swap_curve, token_a_amount, token_b_amount);



        accounts.initialize_swap().unwrap();

        let (
            token_a_key,
            mut token_a_account,
            token_b_key,
            mut token_b_account,
            pool_key,
            mut pool_account,
        ) = accounts.setup_token_accounts(
            &user_key,
            &withdrawer_key,
            initial_a,
            initial_b,
            initial_pool.try_into().unwrap(),
        );

        accounts
            .withdraw_all_token_types(
                &withdrawer_key,
                &pool_key,
                &mut pool_account,
                &token_a_key,
                &mut token_a_account,
                &token_b_key,
                &mut token_b_account,
                withdraw_amount.try_into().unwrap(),
                minimum_token_a_amount,
                minimum_token_b_amount,
            )
            .unwrap();
    }*/
}