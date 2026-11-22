//! One reader for the terminal, shared by whoever needs lines from it
//! (the consent prompt, host-side chat).

use std::sync::OnceLock;

use tokio::io::AsyncBufReadExt;
use tokio::sync::broadcast;

static LINES: OnceLock<broadcast::Sender<String>> = OnceLock::new();

/// Subscribe to lines typed into the terminal from now on.
pub fn lines() -> broadcast::Receiver<String> {
    LINES
        .get_or_init(|| {
            let (tx, _) = broadcast::channel(64);
            let t = tx.clone();
            tokio::spawn(async move {
                let mut r = tokio::io::BufReader::new(tokio::io::stdin()).lines();
                while let Ok(Some(l)) = r.next_line().await {
                    let _ = t.send(l);
                }
            });
            tx
        })
        .subscribe()
}
