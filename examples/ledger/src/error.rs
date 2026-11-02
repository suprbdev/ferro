#[derive(Debug, thiserror::Error)]
pub enum LedgerError {
    #[error("unbalanced entry in {currency}: debits {debits} != credits {credits}")]
    Unbalanced {
        currency: String,
        debits: String,
        credits: String,
    },
    #[error("entry needs at least two lines")]
    TooFewLines,
    #[error("entry has too many lines")]
    TooManyLines,
    #[error("line amount must be non-zero")]
    ZeroAmount,
    #[error("no such account: {0}")]
    NoAccount(String),
    #[error("account already exists: {0}")]
    AccountExists(String),
    #[error("account is closed: {0}")]
    AccountClosed(String),
    #[error("account {0} has a non-zero balance and cannot be closed")]
    NonZeroBalance(String),
    #[error("currency mismatch on {account}: account is {expected}, line is {given}")]
    CurrencyMismatch {
        account: String,
        expected: String,
        given: String,
    },
    #[error("period is closed through {through}: cannot post dated {date}")]
    PeriodClosed { through: String, date: String },
    #[error(
        "adjusting entries are dated after {through} and adjust a date inside the closed period"
    )]
    BadAdjustment { through: String },
    #[error("no closed period to adjust")]
    NoClosedPeriod,
    #[error("period already closed through {0}")]
    PeriodAlreadyClosed(String),
    #[error("no such entry: #{0}")]
    NoEntry(u64),
    #[error("entry #{0} is already reversed by #{1}")]
    AlreadyReversed(u64, u64),
    #[error("amount overflow")]
    Overflow,
    #[error("invalid amount `{0}`")]
    BadAmount(String),
    #[error("invalid date `{0}`, expected YYYY-MM-DD")]
    BadDate(String),
    #[error("invalid line `{0}`, expected ACCOUNT=AMOUNT[ CURRENCY]")]
    BadLine(String),
    #[error("{0}")]
    Usage(String),
    #[error(transparent)]
    Db(#[from] ferro::Error),
}

pub type Result<T> = std::result::Result<T, LedgerError>;
