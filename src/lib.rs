use anyhow::anyhow;
use kitsune::{
    activitypub::Fetcher,
    cache::{Cache, NoopCache},
    config::Configuration,
    mapping::MastodonMapper,
    resolve::PostResolver,
    service::{
        account::AccountService, oauth2::Oauth2Service, post::PostService,
        search::NoopSearchService, timeline::TimelineService, user::UserService,
    },
    state::{Service, Zustand},
    webfinger::Webfinger,
};
use migration::{Migrator, MigratorTrait};
use sea_orm::SqlxPostgresConnector;
use shuttle_secrets::{SecretStore, Secrets};
use shuttle_service::ShuttleAxum;
use shuttle_shared_db::Postgres;
use sqlx::PgPool;
use std::{num::NonZeroUsize, path::PathBuf, sync::Arc};
use sync_wrapper::SyncWrapper;

#[shuttle_service::main]
async fn axum(#[Postgres] db_conn: PgPool, #[Secrets] secret_store: SecretStore) -> ShuttleAxum {
    let config = Configuration {
        database_url: String::new(),
        domain: secret_store
            .get("DOMAIN")
            .ok_or_else(|| anyhow!("Domain not set"))?,
        frontend_dir: PathBuf::new(),
        job_workers: NonZeroUsize::new(5).unwrap(),
        port: 0,
        redis_url: String::new(),
        prometheus_port: 0,
        search_index_server: String::new(),
        search_servers: vec![],
        upload_dir: PathBuf::from("uploads"),
    };

    let db_conn = SqlxPostgresConnector::from_sqlx_postgres_pool(db_conn);
    Migrator::up(&db_conn, None)
        .await
        .map_err(anyhow::Error::from)?;

    let search_service = Arc::new(NoopSearchService);

    let fetcher: Fetcher = Fetcher::new(
        db_conn.clone(),
        search_service.clone(),
        Arc::new(NoopCache),
        Arc::new(NoopCache),
    );
    let webfinger = Webfinger::new(Arc::new(NoopCache) as Arc<dyn Cache<_, _> + Send + Sync>);

    let account_service = AccountService::builder()
        .db_conn(db_conn.clone())
        .build()
        .map_err(anyhow::Error::from)?;

    let oauth2_service = Oauth2Service::builder()
        .db_conn(db_conn.clone())
        .build()
        .map_err(anyhow::Error::from)?;

    let post_resolver = PostResolver::new(db_conn.clone(), fetcher.clone(), webfinger.clone());
    let post_service = PostService::builder()
        .config(config.clone())
        .db_conn(db_conn.clone())
        .post_resolver(post_resolver)
        .search_service(search_service.clone())
        .build()
        .map_err(anyhow::Error::from)?;

    let timeline_service = TimelineService::builder()
        .db_conn(db_conn.clone())
        .build()
        .map_err(anyhow::Error::from)?;

    let user_service = UserService::builder()
        .config(config.clone())
        .db_conn(db_conn.clone())
        .build()
        .map_err(anyhow::Error::from)?;

    let mastodon_mapper = MastodonMapper::builder()
        .db_conn(db_conn.clone())
        .mastodon_cache(Arc::new(NoopCache))
        .build()
        .map_err(anyhow::Error::from)?;

    let state = Zustand {
        config: config.clone(),
        db_conn,
        fetcher,
        mastodon_mapper,
        service: Service {
            account: account_service,
            oauth2: oauth2_service,
            search: search_service,
            post: post_service,
            timeline: timeline_service,
            user: user_service,
        },
        webfinger,
    };

    for _ in 0..config.job_workers.get() {
        tokio::spawn(kitsune::job::run(state.clone()));
    }

    let router = kitsune::http::router(state);
    let sync_wrapper = SyncWrapper::new(router);

    Ok(sync_wrapper)
}
