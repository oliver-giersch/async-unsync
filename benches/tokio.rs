#![feature(test)]

extern crate test;

use test::Bencher;

#[cfg(feature = "bench")]
fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread().worker_threads(6).build().unwrap()
}

#[cfg(feature = "bench")]
#[bench]
fn uncontented_bounded_sync(b: &mut Bencher) {
    let rt = rt();

    b.iter(|| {
        rt.block_on(async move {
            let (tx, mut rx) = tokio::sync::mpsc::channel::<usize>(1_000_000);

            for i in 0..5000 {
                tx.send(i).await.unwrap();
            }

            for _ in 0..5_000 {
                let _ = rx.recv().await;
            }
        })
    });
}

#[cfg(feature = "bench")]
#[bench]
fn uncontented_unbounded_sync(b: &mut Bencher) {
    let rt = rt();

    b.iter(|| {
        rt.block_on(async move {
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<usize>();

            for i in 0..5000 {
                tx.send(i).unwrap();
            }

            for _ in 0..5_000 {
                let _ = rx.recv().await;
            }
        })
    });
}

#[cfg(feature = "bench")]
#[bench]
fn uncontented_bounded_unsync(b: &mut Bencher) {
    let rt = rt();

    b.iter(|| {
        rt.block_on(async move {
            let (tx, mut rx) = async_unsync::bounded::channel::<usize>(1_000_000).into_split();

            for i in 0..5000 {
                tx.send(i).await.unwrap();
            }

            for _ in 0..5_000 {
                let _ = rx.recv().await;
            }
        })
    });
}

#[cfg(feature = "bench")]
#[bench]
fn uncontented_unbounded_unsync(b: &mut Bencher) {
    let rt = rt();

    b.iter(|| {
        rt.block_on(async move {
            let (tx, mut rx) = async_unsync::unbounded::channel::<usize>().into_split();

            for i in 0..5000 {
                tx.send(i).unwrap();
            }

            for _ in 0..5_000 {
                let _ = rx.recv().await;
            }
        })
    });
}
