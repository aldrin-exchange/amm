//! Creates a new [`Pool`] account. This endpoint is generic and can be used for
//! constant product curve, in which case the amplifier input is going to be
//! zero, and for stable curve.
//!
//! The number of remaining accounts determine how many reserves does the pool
//! have, ie. for multi-asset pools provide up to 4 remaining accounts.
//!
//! The remaining accounts must be vaults, ie. token accounts owned by the pool
//! signers. The order of the accounts does not matter.

use crate::prelude::*;
use anchor_lang::solana_program::program_option::COption;
use anchor_spl::token::{Mint, Token, TokenAccount};
use std::collections::BTreeSet;

#[derive(Accounts)]
pub struct CreatePool<'info> {
    #[account(mut)]
    pub admin: Signer<'info>,
    #[account(
        init,
        payer = admin,
        space = Pool::space()
    )]
    pub pool: Account<'info, Pool>,
    /// CHECK: UNSAFE_CODES.md#signer
    #[account(
        seeds = [Pool::SIGNER_PDA_PREFIX, pool.key().as_ref()],
        bump
    )]
    pub pool_signer: AccountInfo<'info>,
    #[account(
        seeds = [ProgramToll::PDA_SEED],
        bump,
    )]
    pub program_toll: Account<'info, ProgramToll>,
    #[account(
        constraint = program_toll_wallet.mint == lp_mint.key()
            @ err::acc("Toll wallet must be of LP mint"),
        constraint = program_toll_wallet.owner == program_toll.authority
            @ err::acc(
                "Toll wallet authority must match \
                program toll authority"
            ),
    )]
    pub program_toll_wallet: Account<'info, TokenAccount>,
    #[account(
        constraint = lp_mint.mint_authority == COption::Some(pool_signer.key())
            @ err::acc("LP mint authority must be the pool signer"),
        constraint = lp_mint.freeze_authority == COption::None
            @ err::acc("LP mint mustn't have a freeze authority"),
    )]
    pub lp_mint: Account<'info, Mint>,
    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
}

pub fn handle(ctx: Context<CreatePool>, amplifier: u64) -> Result<()> {
    let accs = ctx.accounts;

    accs.pool.mint = accs.lp_mint.key();
    accs.pool.admin = accs.admin.key();
    accs.pool.signer = accs.pool_signer.key();
    accs.pool.curve = if amplifier == 0 {
        Curve::ConstProd
    } else {
        Curve::Stable {
            amplifier,
            invariant: SDecimal::default(),
        }
    };

    if ctx.remaining_accounts.len() > consts::MAX_RESERVES {
        return Err(error!(err::acc("Too many reserves")));
    }

    let is_lp_mint_without_supply = accs.lp_mint.supply == 0;
    let mut mints = BTreeSet::new();
    for (index, vault_info) in ctx.remaining_accounts.iter().enumerate() {
        let vault = Account::<TokenAccount>::try_from(vault_info)?;

        if is_lp_mint_without_supply && vault.amount != 0 {
            // if there are no minted LP tokens, then vaults must be empty
            return Err(error!(err::acc(
                "If LP mint supply is zero, then vault amount must too"
            )));
        } else if !is_lp_mint_without_supply && vault.amount == 0 {
            // if there are LP tokens minted already, then vaults must be empty,
            // otherwise the admin could have minted bunch of LP tokens up front
            // and then just redeem them when liquidity was deposited
            return Err(error!(err::acc(
                "If LP mint supply is not zero then vault amount mustn't either"
            )));
        }

        if mints.contains(&vault.mint) {
            return Err(error!(err::acc("Duplicate reserve mint")));
        }
        if vault.close_authority.is_some() {
            return Err(error!(err::acc(
                "Vault mustn't have a close authority"
            )));
        }
        if vault.delegate.is_some() {
            return Err(error!(err::acc("Vault mustn't have a delegate")));
        }
        if vault.owner != accs.pool_signer.key() {
            return Err(error!(err::acc("Vault owner must be pool signer")));
        }
        if vault.is_frozen() {
            return Err(error!(err::acc("Vault mustn't be frozen")));
        }

        mints.insert(vault.mint);
        accs.pool.reserves[index] = Reserve {
            vault: vault_info.key(),
            mint: vault.mint,
            tokens: TokenAmount::new(vault.amount),
        };
    }

    if mints.len() < 2 {
        return Err(error!(err::acc("At least 2 vaults must be provided")));
    }

    accs.pool.dimension = mints.len() as u64;
    accs.pool.program_toll_wallet = accs.program_toll_wallet.key();

    Ok(())
}
