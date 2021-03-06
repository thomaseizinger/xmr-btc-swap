pub mod testutils;

use swap::protocol::{alice, bob};
use testutils::SlowCancelConfig;
use tokio::join;

/// Run the following tests with RUST_MIN_STACK=10000000

#[tokio::test]
async fn happy_path() {
    testutils::setup_test(SlowCancelConfig, |mut ctx| async move {
        let (bob_swap, _) = ctx.bob_swap().await;
        let bob_swap = tokio::spawn(bob::run(bob_swap));

        let alice_swap = ctx.alice_next_swap().await;
        let alice_swap = tokio::spawn(alice::run(alice_swap));

        let (bob_state, alice_state) = join!(bob_swap, alice_swap);

        ctx.assert_alice_redeemed(alice_state??).await;
        ctx.assert_bob_redeemed(bob_state??).await;

        Ok(())
    })
    .await;
}
