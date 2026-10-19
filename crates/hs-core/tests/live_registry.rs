//! The published node images on ghcr.io, read without a token.

#![allow(clippy::unwrap_used)]

use hs_core::registry::recent;

#[tokio::test]
#[ignore = "reads ghcr.io"]
async fn the_last_images_come_newest_first_with_their_times() {
    let current = "ghcr.io/hipotures/homestack-server:0.1.0-d104adebb140".to_owned();
    let found = recent(
        "ghcr.io/hipotures/homestack-server:any",
        5,
        std::slice::from_ref(&current),
    )
    .await
    .unwrap();
    for image in &found {
        println!("{} {:?}", image.image, image.created);
    }
    assert_eq!(found.len(), 6, "{found:?}");
    assert!(found.iter().all(|image| image.created.is_some()), "{found:?}");
    assert!(
        found.windows(2).all(|pair| pair[0].created >= pair[1].created),
        "{found:?}"
    );
    assert!(found.iter().any(|image| image.image == current), "{found:?}");
}
