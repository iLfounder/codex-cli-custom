use std::fmt;

const ACCOUNT_PREFIX: char = 'C';

#[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct AccountId(u32);

impl AccountId {
    pub(crate) fn parse(value: &str) -> Option<Self> {
        let number = value.strip_prefix(ACCOUNT_PREFIX)?.parse::<u32>().ok()?;
        (number > 0 && value == format!("{ACCOUNT_PREFIX}{number}")).then_some(Self(number))
    }

    pub(crate) const fn number(self) -> u32 {
        self.0
    }
}

impl fmt::Debug for AccountId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl fmt::Display for AccountId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{ACCOUNT_PREFIX}{}", self.0)
    }
}

#[cfg(test)]
#[path = "identity_tests.rs"]
mod tests;
