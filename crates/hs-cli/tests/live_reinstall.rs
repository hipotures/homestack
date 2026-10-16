//! The reinstall box's reading on the real cluster: the image each server
//! container was made from (deploy token, read only) and the registry's.

#![allow(clippy::unwrap_used)]

#[tokio::test]
#[ignore = "reads the cluster and ghcr.io with ~/.config/homestack/deploy.toml"]
async fn the_servers_images_and_the_published_ones_are_read() {
    let home = std::env::var("HOME").unwrap();
    let deploy_toml = std::path::Path::new(&home).join(".config/homestack/deploy.toml");
    let config = hs_deploy::config::DeployConfig::load(&deploy_toml).unwrap();
    let vmids: Vec<u32> = config.instances.iter().map(|instance| instance.ctid).collect();
    let images = hs_cli::tui::reinstall::read(&deploy_toml, &vmids, 7200).await.unwrap();
    for server in &images.servers {
        println!(
            "{} {:?}",
            server.name,
            server.image.as_ref().map(|image| (image.tag(), &image.made))
        );
    }
    for image in &images.published {
        println!("published {} {}", image.tag(), image.made);
    }
    assert_eq!(images.servers.len(), vmids.len());
    assert!(!images.published.is_empty());
}
