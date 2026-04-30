/// A communicator describes a fixed-size group of peers, each with a unique rank.
///
/// Both `MpiCommunicator` and `UcxCommunicator` implement this trait.
pub trait Communicator {
    /// This peer's rank in `[0, size)`.
    fn rank(&self) -> usize;
    /// Total number of peers in the communicator.
    fn size(&self) -> usize;
}
