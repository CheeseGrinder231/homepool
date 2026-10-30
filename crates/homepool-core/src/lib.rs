//! Pure types shared by the applications and adapters.

/// An opaque identifier for a member in the pool.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MemberId(pub u64);

#[cfg(test)]
mod tests {
    use super::MemberId;

    #[test]
    fn member_identity_is_typed() {
        assert_eq!(MemberId(7), MemberId(7));
        assert_ne!(MemberId(7), MemberId(8));
    }
}
