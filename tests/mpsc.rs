#[cfg(feature = "alloc")]
#[test]
fn mpsc() {
    use futures_lite::future;

    future::block_on(async {
        let mut chan = async_unsync::bounded::channel(1);
        let (tx, mut rx) = chan.split();

        let f1 = future::zip(push_loop(tx.clone(), 1), push_loop(tx, 2));
        let f2 = async move {
            let mut vec = vec![];
            while let Some(id) = rx.recv().await {
                vec.push(id);
                if vec.len() == 10 {
                    rx.close();
                }
            }

            vec
        };

        let (_, res) = future::zip(f1, f2).await;
        assert_eq!(res, &[1, 2, 1, 2, 1, 2, 1, 2, 1, 2]);
    })
}

#[cfg(feature = "alloc")]
async fn push_loop(tx: async_unsync::bounded::SenderRef<'_, i32>, id: i32) {
    let mut count = 0;
    while let Ok(_) = tx.send(id).await {
        futures_lite::future::yield_now().await;
        count += 1;
    }

    assert_eq!(count, 5);
}
