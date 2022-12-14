use crate::prelude::*;
use std::fmt::Display;

#[error_code]
pub enum FarmingError {
    #[msg("Operation would result in an overflow")]
    MathOverflow,
    /// Use this error via the [`acc`] function to provide more background
    /// about the issue.
    #[msg("Provided account breaks some constraints, see logs for more info")]
    InvalidAccountInput,
    /// Use this error for program paths which should never be reached if the
    /// program logic works as intended.
    #[msg("There's a bug in the program, see logs for more info")]
    InvariantViolation,
    #[msg("One of the provided input arguments is invalid")]
    InvalidArg,
    #[msg("Farm admin does not match the provided signer")]
    FarmAdminMismatch,
    #[msg("Insufficient slot time has passed since last snapshot was taken")]
    InsufficientSlotTimeSinceLastSnapshot,
    #[msg("None of existing harvest mints  possedes the public key")]
    UnknownHarvestMintPubKey,
    #[msg("The limit of configuration updates has been already exceeded within the snapshot history")]
    ConfigurationUpdateLimitExceeded,
    #[msg(
        "Stake mint must be one of harvestable mints \
        for autocompouding to work"
    )]
    CannotCompoundIfStakeMintIsNotHarvest,
    #[msg("Running harvest periods must finish before a new one can start")]
    CannotOverwriteOpenHarvestPeriod,
    #[msg("Harvest period must start before it ends")]
    HarvestPeriodCannotHaveNegativeLength,
    #[msg("Cannot start a new harvest period in the past")]
    HarvestPeriodMustStartAtOrAfterCurrentSlot,
    #[msg("Cannot have a period that lasts 0 slots")]
    HarvestPeriodMustBeAtLeastOneSlot,
}

pub fn acc(msg: impl Display) -> FarmingError {
    msg!("[InvalidAccountInput] {}", msg);

    FarmingError::InvalidAccountInput
}

pub fn arg(msg: impl Display) -> FarmingError {
    msg!("[InvalidArg] {}", msg);

    FarmingError::InvalidArg
}
