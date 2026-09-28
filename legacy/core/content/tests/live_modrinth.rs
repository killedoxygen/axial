use axial_content::{CanonicalId, ContentKind, ContentQuery, ContentService, LoaderGameFilter};

fn service() -> ContentService {
    ContentService::new(reqwest::Client::new())
}

#[tokio::test]
#[ignore = "hits the live Modrinth API"]
async fn search_detail_versions_identify_roundtrip() {
    let service = service();

    let mut query = ContentQuery::new(ContentKind::Mod);
    query.search = Some("sodium".to_string());
    query.loader = Some("fabric".to_string());
    query.game_version = Some("1.21.6".to_string());
    query.limit = 5;

    let page = service.search(&query).await.expect("search");
    assert!(!page.items.is_empty(), "expected search hits");
    let first = &page.items[0];
    println!(
        "search[0]: {} by {} ({} downloads) id={}",
        first.title,
        first.author,
        first.downloads,
        first.canonical_id.as_str()
    );

    let sodium = CanonicalId::for_project(axial_content::ProviderId::Modrinth, "AANobbMI");
    let detail = service.detail(&sodium).await.expect("detail");
    println!(
        "detail: {} — {} versions, {} gallery",
        detail.content.title,
        detail.versions.len(),
        detail.gallery.len()
    );
    assert_eq!(detail.content.title.to_lowercase(), "sodium");

    let versions = service
        .versions(
            &sodium,
            &LoaderGameFilter {
                loader: Some("fabric".to_string()),
                game_version: Some("1.21.6".to_string()),
            },
        )
        .await
        .expect("versions");
    assert!(!versions.is_empty(), "expected fabric 1.21.6 versions");
    let file = versions[0].primary_file().expect("primary file");
    println!(
        "version: {} file={} sha512={:?}",
        versions[0].version_number,
        file.filename,
        file.sha512.as_deref().map(|hash| &hash[..12])
    );
    let sha512 = file.sha512.clone().expect("sha512 present");

    let identified = service
        .identify(std::slice::from_ref(&sha512))
        .await
        .expect("identify");
    let identity = identified.get(&sha512).expect("hash identified");
    assert_eq!(identity.project_id, "AANobbMI");
    println!("identify: {} -> {}", &sha512[..12], identity.project_id);
}
