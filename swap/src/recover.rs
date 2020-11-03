use crate::state::{Alice, Bob, Swap};

pub struct Error;

pub async fn recover(state: Swap) -> Result<(), Error> {
    match state {
        Swap::Alice(state) => alice_recover(state).await,
        Swap::Bob(state) => bob_recover(state).await,
    }
}

pub async fn alice_recover(_state: Alice) -> Result<(), Error> {
    todo!()
}

pub async fn bob_recover(_state: Bob) -> Result<(), Error> {
    todo!()
}
