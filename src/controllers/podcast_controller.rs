use crate::constants::inner_constants::{
    PodcastType, BASIC_AUTH, COMMON_USER_AGENT, DEFAULT_IMAGE_URL, ENVIRONMENT_SERVICE, MAIN_ROOM,
    OIDC_AUTH,
};
use crate::models::dto_models::PodcastFavorUpdateModel;
use crate::models::misc_models::{PodcastAddModel, PodcastInsertModel};
use crate::models::opml_model::OpmlModel;
use crate::models::search_type::SearchType::{ITunes, Podindex};
use crate::service::podcast_episode_service::PodcastEpisodeService;
use crate::service::rust_service::PodcastService;
use crate::{get_default_image, unwrap_string};
use actix_web::dev::PeerAddr;
use actix_web::http::Method;
use actix_web::web::{Data, Json, Path};
use actix_web::{delete, error, get, post, put, Error, HttpRequest};
use actix_web::{web, HttpResponse};
use async_recursion::async_recursion;
use opml::{Outline, OPML};
use rand::rngs::ThreadRng;
use rand::Rng;
use rss::Channel;
use serde_json::{from_str, Value};
use std::thread;
use tokio::task::spawn_blocking;

use crate::models::filter::Filter;
use crate::models::messages::BroadcastMessage;
use crate::models::order_criteria::{OrderCriteria, OrderOption};
use crate::models::podcast_episode::PodcastEpisode;
use crate::models::podcast_rssadd_model::PodcastRSSAddModel;
use crate::models::podcasts::Podcast;
use crate::models::user::User;
use crate::service::file_service::{perform_podcast_variable_replacement, FileService};
use crate::utils::append_to_header::add_basic_auth_headers_conditionally;
use futures_util::StreamExt;
use reqwest::header::HeaderMap;
use tokio::runtime::Runtime;
use tokio::sync::mpsc;
use tokio_stream::wrappers::UnboundedReceiverStream;

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PodcastSearchModel {
    order: Option<OrderCriteria>,
    title: Option<String>,
    order_option: Option<OrderOption>,
    favored_only: bool,
    tag: Option<String>,
}

#[utoipa::path(
context_path="/api/v1",
responses(
(status = 200, description = "Gets the user specific filter.",body= Option<Filter>)),
tag="podcasts"
)]
#[get("/podcasts/filter")]
pub async fn get_filter(
    requester: web::ReqData<User>,
) -> Result<HttpResponse, CustomError> {
    let filter = Filter::get_filter_by_username(&requester.username).await?;
    Ok(HttpResponse::Ok().json(filter))
}

#[utoipa::path(
context_path="/api/v1",
responses(
(status = 200, description = "Gets the podcasts matching the searching criteria",body=
Vec<PodcastDto>)),
tag="podcasts"
)]
#[get("/podcasts/search")]
pub async fn search_podcasts(
    query: web::Query<PodcastSearchModel>,
    requester: web::ReqData<User>,
) -> Result<HttpResponse, CustomError> {
    let query = query.into_inner();
    let _order = query.order.unwrap_or(OrderCriteria::Asc);
    let _latest_pub = query.order_option.unwrap_or(OrderOption::Title);
    let tag = query.tag;

    let opt_filter =
        Filter::get_filter_by_username(&requester.username).await?;

    let only_favored = match opt_filter {
        Some(filter) => filter.only_favored,
        None => true,
    };

    let username = requester.username.clone();
    let filter = Filter::new(
        username.clone(),
        query.title.clone(),
        _order.clone().to_bool(),
        Some(_latest_pub.clone().to_string()),
        only_favored,
    );
    Filter::save_filter(filter)?;

    match query.favored_only {
        true => {
            let podcasts;
            {
                podcasts = PodcastService::search_podcasts_favored(
                    _order.clone(),
                    query.title,
                    _latest_pub.clone(),
                    username,
                    tag,
                )?;
            }
            Ok(HttpResponse::Ok().json(podcasts))
        }
        false => {
            let podcasts;
            {
                podcasts = PodcastService::search_podcasts(
                    _order.clone(),
                    query.title,
                    _latest_pub.clone(),
                    username,
                    tag,
                )?;
            }
            Ok(HttpResponse::Ok().json(podcasts))
        }
    }
}

#[utoipa::path(
context_path="/api/v1",
responses(
(status = 200, description = "Find a podcast by its collection id", body = [PodcastDto])
),
tag="podcasts"
)]
#[get("/podcast/{id}")]
pub async fn find_podcast_by_id(
    id: Path<String>,
    user: web::ReqData<User>,
) -> Result<HttpResponse, CustomError> {
    let id_num = from_str::<i32>(&id).unwrap();
    let username = &user.username;

    let podcast = PodcastService::get_podcast(id_num)?;
    let tags = Tag::get_tags_of_podcast(id_num, username)?;
    let favorite = Favorite::get_favored_podcast_by_username_and_podcast_id(username, id_num)?;
    let podcast_dto: PodcastDto = (podcast, favorite, tags).into();
    Ok(HttpResponse::Ok().json(podcast_dto))
}

#[utoipa::path(
context_path="/api/v1",
responses(
(status = 200, description = "Gets all stored podcasts as a list", body = [PodcastDto])
),
tag="podcasts"
)]
#[get("/podcasts")]
pub async fn find_all_podcasts(
    requester: web::ReqData<User>,
) -> Result<HttpResponse, CustomError> {
    let username = &requester.username;

    let podcasts = PodcastService::get_podcasts(username)?;

    Ok(HttpResponse::Ok().json(podcasts))
}
use crate::models::itunes_models::ItunesModel;
#[utoipa::path(
context_path="/api/v1",
responses(
(status = 200, description = "Finds a podcast from the itunes url.", body = [ItunesModel])
),
tag="podcasts"
)]
#[get("/podcasts/{type_of}/{podcast}/search")]
pub async fn find_podcast(
    podcast_col: Path<(i32, String)>,
    requester: web::ReqData<User>,
) -> Result<HttpResponse, CustomError> {
    if !requester.is_privileged_user() {
        return Err(CustomError::Forbidden);
    }

    let (type_of, podcast) = podcast_col.into_inner();
    match type_of.try_into() {
        Ok(ITunes) => {
            let res;
            {
                log::debug!("Searching for podcast: {}", podcast);
                res = PodcastService::find_podcast(&podcast).await;
            }
            Ok(HttpResponse::Ok().json(res))
        }
        Ok(Podindex) => {
            if !ENVIRONMENT_SERVICE.get_config().podindex_configured {
                return Ok(HttpResponse::BadRequest().json("Podindex is not configured"));
            }

            Ok(HttpResponse::Ok().json(PodcastService::find_podcast_on_podindex(&podcast).await?))
        }
        Err(_) => Err(CustomError::BadRequest("Invalid search type".to_string())),
    }
}

#[utoipa::path(
context_path="/api/v1",
request_body=PodcastAddModel,
responses(
(status = 200, description = "Adds a podcast to the database.")),
tag="podcasts"
)]
#[post("/podcast/itunes")]
pub async fn add_podcast(
    track_id: web::Json<PodcastAddModel>,
    lobby: Data<ChatServerHandle>,
    requester: web::ReqData<User>,
) -> Result<HttpResponse, CustomError> {
    if !requester.is_privileged_user() {
        return Err(CustomError::Forbidden);
    }

    let query: Vec<(&str, String)> = vec![
        ("id", track_id.track_id.to_string()),
        ("entity", "podcast".to_string()),
    ];

    let res = get_http_client()
        .get("https://itunes.apple.com/lookup")
        .query(&query)
        .send()
        .await
        .unwrap();

    let res = res.json::<Value>().await.unwrap();

    PodcastService::handle_insert_of_podcast(
        PodcastInsertModel {
            feed_url: unwrap_string(&res["results"][0]["feedUrl"]),
            title: unwrap_string(&res["results"][0]["collectionName"]),
            id: unwrap_string(&res["results"][0]["collectionId"])
                .parse()
                .unwrap(),
            image_url: unwrap_string(&res["results"][0]["artworkUrl600"]),
        },
        lobby,
        None,
    )
    .await?;
    Ok(HttpResponse::Ok().into())
}

#[utoipa::path(
context_path="/api/v1",
responses(
(status = 200, description = "Adds a podcast by its feed url",body=PodcastDto)),
tag="podcasts"
)]
#[post("/podcast/feed")]
pub async fn add_podcast_by_feed(
    rss_feed: web::Json<PodcastRSSAddModel>,
    lobby: Data<ChatServerHandle>,
    requester: web::ReqData<User>,
) -> Result<HttpResponse, CustomError> {
    if !requester.is_privileged_user() {
        return Err(CustomError::Forbidden);
    }
    let mut header_map = HeaderMap::new();
    header_map.insert("User-Agent", COMMON_USER_AGENT.parse().unwrap());
    add_basic_auth_headers_conditionally(rss_feed.clone().rss_feed_url, &mut header_map);
    let result = get_http_client()
        .get(rss_feed.clone().rss_feed_url)
        .headers(header_map)
        .send()
        .await
        .map_err(map_reqwest_error)?;

    let bytes = result.text().await.unwrap();

    let channel = Channel::read_from(bytes.as_bytes()).unwrap();
    let num = rand::thread_rng().gen_range(100..10000000);

    let res: PodcastDto;
    {
        res = PodcastService::handle_insert_of_podcast(
            PodcastInsertModel {
                feed_url: rss_feed.clone().rss_feed_url.clone(),
                title: channel.title.clone(),
                id: num,
                image_url: channel
                    .image
                    .clone()
                    .map(|i| i.url)
                    .unwrap_or(get_default_image()),
            },
            lobby,
            Some(channel),
        )
        .await?
        .into();
    }

    Ok(HttpResponse::Ok().json(res))
}

#[utoipa::path(
context_path="/api/v1",
request_body=OpmlModel,
responses(
(status = 200, description = "Adds all podcasts of an opml podcast list to the database.")),
tag="podcasts"
)]
#[post("/podcast/opml")]
pub async fn import_podcasts_from_opml(
    opml: Json<OpmlModel>,
    lobby: Data<ChatServerHandle>,
    requester: web::ReqData<User>,
) -> Result<HttpResponse, CustomError> {
    if !requester.is_privileged_user() {
        return Err(CustomError::Forbidden);
    }
    let document = OPML::from_str(&opml.content).unwrap();

    spawn_blocking(move || {
        for outline in document.body.outlines {
            let moved_lobby = lobby.clone();
            thread::spawn(move || {
                let rt = Runtime::new().unwrap();
                let rng = rand::thread_rng();
                rt.block_on(insert_outline(outline.clone(), moved_lobby, rng.clone()));
            });
        }
    });

    Ok(HttpResponse::Ok().into())
}

#[utoipa::path(
context_path="/api/v1",
request_body=PodcastAddModel,
responses(
(status = 200, description = "Adds a podindex podcast to the database")),
tag="podcasts"
)]
#[post("/podcast/podindex")]
pub async fn add_podcast_from_podindex(
    id: web::Json<PodcastAddModel>,
    lobby: Data<ChatServerHandle>,
    requester: web::ReqData<User>,
) -> Result<HttpResponse, CustomError> {
    if !requester.is_privileged_user() {
        return Err(CustomError::Forbidden);
    }

    if !ENVIRONMENT_SERVICE.get_config().podindex_configured {
        return Err(CustomError::BadRequest(
            "Podindex is not configured".to_string(),
        ));
    }

    spawn_blocking(move || match start_download_podindex(id.track_id, lobby) {
        Ok(_) => {}
        Err(e) => {
            log::error!("Error: {}", e)
        }
    });
    Ok(HttpResponse::Ok().into())
}

fn start_download_podindex(id: i32, lobby: Data<ChatServerHandle>) -> Result<Podcast, CustomError> {
    let rt = Runtime::new().unwrap();

    rt.block_on(async { PodcastService::insert_podcast_from_podindex(id, lobby).await })
}

#[utoipa::path(
context_path="/api/v1",
params(("podcast", description="The podcast episode query parameter.")),
responses(
(status = 200, description = "Queries for a podcast episode by a query string", body = Vec<PodcastEpisode>)),
tag="podcasts",)]
#[get("/podcasts/{podcast}/query")]
pub async fn query_for_podcast(podcast: Path<String>) -> Result<HttpResponse, CustomError> {
    let res = PodcastEpisodeService::query_for_podcast(&podcast)?;

    Ok(HttpResponse::Ok().json(res))
}

#[utoipa::path(
context_path="/api/v1",
responses(
(status = 200, description = "Refreshes all podcasts")),
tag="podcasts"
)]
#[post("/podcast/all")]
pub async fn refresh_all_podcasts(
    lobby: Data<ChatServerHandle>,
    requester: web::ReqData<User>,
) -> Result<HttpResponse, CustomError> {
    if !requester.is_privileged_user() {
        return Err(CustomError::Forbidden);
    }

    let podcasts = Podcast::get_all_podcasts()?;
    thread::spawn(move || {
        for podcast in podcasts {
            PodcastService::refresh_podcast(podcast.clone(), lobby.clone()).unwrap();
            let podcast_dto: PodcastDto = podcast.clone().into();
            lobby.send_broadcast_sync(
                MAIN_ROOM.parse().unwrap(),
                serde_json::to_string(&BroadcastMessage {
                    podcast_episode: None,
                    type_of: PodcastType::RefreshPodcast,
                    message: format!("Refreshed podcast: {}", &podcast.name),
                    podcast: Option::from(podcast_dto),
                    podcast_episodes: None,
                })
                .unwrap(),
            );
        }
    });
    Ok(HttpResponse::Ok().into())
}

#[utoipa::path(
context_path="/api/v1",
responses(
(status = 200, description = "Refreshes a podcast episode")),
tag="podcasts"
)]
#[post("/podcast/{id}/refresh")]
pub async fn download_podcast(
    id: Path<String>,
    lobby: Data<ChatServerHandle>,
    requester: web::ReqData<User>,
) -> Result<HttpResponse, CustomError> {
    if !requester.is_privileged_user() {
        return Err(CustomError::Forbidden);
    }

    let id_num = from_str::<i32>(&id).unwrap();
    let podcast = PodcastService::get_podcast_by_id(id_num);
    thread::spawn(move || {
        match PodcastService::refresh_podcast(podcast.clone(), lobby.clone()) {
            Ok(_) => {
                log::info!("Succesfully refreshed podcast.");
            }
            Err(e) => {
                log::error!("Error refreshing podcast: {}", e);
            }
        }

        let download = PodcastService::schedule_episode_download(podcast.clone(), Some(lobby));

        if download.is_err() {
            log::error!("Error downloading podcast: {}", download.err().unwrap());
        }
    });

    Ok(HttpResponse::Ok().json("Refreshing podcast"))
}

#[utoipa::path(
context_path="/api/v1",
request_body=PodcastFavorUpdateModel,
responses(
(status = 200, description = "Updates favoring a podcast.", body=String)),
tag="podcasts"
)]
#[put("/podcast/favored")]
pub async fn favorite_podcast(
    update_model: Json<PodcastFavorUpdateModel>,
    requester: web::ReqData<User>,
) -> Result<HttpResponse, CustomError> {
    PodcastService::update_favor_podcast(
        update_model.id,
        update_model.favored,
        &requester.username,
    )?;
    Ok(HttpResponse::Ok().json("Favorited podcast"))
}

#[utoipa::path(
context_path="/api/v1",
responses(
(status = 200, description = "Finds all favored podcasts.")),
tag="podcasts"
)]
#[get("/podcasts/favored")]
pub async fn get_favored_podcasts(
    requester: web::ReqData<User>,
) -> Result<HttpResponse, CustomError> {
    let podcasts = PodcastService::get_favored_podcasts(requester.username.clone())?;
    Ok(HttpResponse::Ok().json(podcasts))
}

#[utoipa::path(
context_path="/api/v1",
responses(
(status = 200, description = "Updates the active state of a podcast. If inactive the podcast \
will not be refreshed automatically.")),
tag="podcasts"
)]
#[put("/podcast/{id}/active")]
pub async fn update_active_podcast(
    id: Path<String>,
    requester: web::ReqData<User>,
) -> Result<HttpResponse, CustomError> {
    if !requester.is_privileged_user() {
        return Err(CustomError::Forbidden);
    }

    let id_num = from_str::<i32>(&id).unwrap();
    PodcastService::update_active_podcast(id_num)?;
    Ok(HttpResponse::Ok().json("Updated active podcast"))
}

#[async_recursion(?Send)]
async fn insert_outline(podcast: Outline, lobby: Data<ChatServerHandle>, mut rng: ThreadRng) {
    if !podcast.outlines.is_empty() {
        for outline_nested in podcast.clone().outlines {
            insert_outline(outline_nested, lobby.clone(), rng.clone()).await;
        }
        return;
    }
    let feed_url = podcast.clone().xml_url;
    if feed_url.is_none() {
        return;
    }

    let feed_response = get_http_client().get(feed_url.unwrap()).send().await;
    if feed_response.is_err() {
        lobby
            .send_broadcast(
                MAIN_ROOM.parse().unwrap(),
                serde_json::to_string(&BroadcastMessage {
                    type_of: PodcastType::OpmlErrored,
                    message: feed_response.err().unwrap().to_string(),
                    podcast: None,
                    podcast_episodes: None,
                    podcast_episode: None,
                })
                .unwrap(),
            )
            .await;
        return;
    }
    let content = feed_response.unwrap().bytes().await.unwrap();

    let channel = Channel::read_from(&content[..]);

    match channel {
        Ok(channel) => {
            let image_url = match channel.image {
                Some(ref image) => image.url.clone(),
                None => {
                    log::info!(
                        "No image found for podcast. Downloading from {}",
                        ENVIRONMENT_SERVICE.server_url.clone().to_owned() + DEFAULT_IMAGE_URL
                    );
                    ENVIRONMENT_SERVICE.server_url.clone().to_owned() + "ui/default.jpg"
                }
            };

            let inserted_podcast = PodcastService::handle_insert_of_podcast(
                PodcastInsertModel {
                    feed_url: podcast.clone().xml_url.expect("No feed url"),
                    title: channel.clone().title.to_string(),
                    id: rng.gen::<i32>(),
                    image_url,
                },
                lobby.clone(),
                Some(channel),
            )
            .await;
            match inserted_podcast {
                Ok(podcast) => {
                    let _ = lobby
                        .send_broadcast(
                            MAIN_ROOM.parse().unwrap(),
                            serde_json::to_string(&BroadcastMessage {
                                type_of: PodcastType::OpmlAdded,
                                message: "Refreshed podcasts".to_string(),
                                podcast: Option::from(<Podcast as Into<PodcastDto>>::into(podcast)),
                                podcast_episodes: None,
                                podcast_episode: None,
                            })
                            .unwrap(),
                        )
                        .await;
                }
                Err(e) => {
                    let _ = lobby
                        .send_broadcast(
                            MAIN_ROOM.parse().unwrap(),
                            serde_json::to_string(&BroadcastMessage {
                                type_of: PodcastType::OpmlErrored,
                                message: e.to_string(),
                                podcast: None,
                                podcast_episodes: None,
                                podcast_episode: None,
                            })
                            .unwrap(),
                        )
                        .await;
                }
            }
        }
        Err(e) => {
            let _ = lobby
                .send_broadcast(
                    MAIN_ROOM.parse().unwrap(),
                    serde_json::to_string(&BroadcastMessage {
                        type_of: PodcastType::OpmlErrored,
                        message: e.to_string(),
                        podcast: None,
                        podcast_episodes: None,
                        podcast_episode: None,
                    })
                    .unwrap(),
                )
                .await;
        }
    }
}
use crate::models::episode::Episode;
use crate::models::tag::Tag;
use utoipa::ToSchema;

use crate::controllers::podcast_episode_controller::EpisodeFormatDto;
use crate::controllers::server::ChatServerHandle;
use crate::controllers::websocket_controller::RSSAPiKey;
use crate::models::favorites::Favorite;
use crate::models::podcast_dto::PodcastDto;
use crate::models::podcast_settings::PodcastSetting;
use crate::models::settings::Setting;
use crate::models::tags_podcast::TagsPodcast;
use crate::utils::environment_variables::is_env_var_present_and_true;

use crate::utils::error::{map_reqwest_error, CustomError};
use crate::utils::http_client::get_http_client;
use crate::utils::rss_feed_parser::PodcastParsed;

#[derive(Deserialize, ToSchema)]
pub struct DeletePodcast {
    pub delete_files: bool,
}

#[utoipa::path(
context_path="/api/v1",
request_body=DeletePodcast,
responses(
(status = 200, description = "Deletes a podcast by id")),
tag="podcasts"
)]
#[delete("/podcast/{id}")]
pub async fn delete_podcast(
    data: web::Json<DeletePodcast>,
    id: Path<i32>,
    requester: web::ReqData<User>,
) -> Result<HttpResponse, CustomError> {
    if !requester.is_privileged_user() {
        return Err(CustomError::Forbidden);
    }

    let podcast = Podcast::get_podcast(*id).expect(
        "Error \
        finding podcast",
    );
    if data.delete_files {
        FileService::delete_podcast_files(&podcast.directory_name);
    }
    Episode::delete_watchtime(*id)?;
    PodcastEpisode::delete_episodes_of_podcast(*id)?;
    TagsPodcast::delete_tags_by_podcast_id(*id)?;

    Podcast::delete_podcast(*id)?;
    Ok(HttpResponse::Ok().into())
}
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Params {
    episode_id: String,
}

#[utoipa::path(
context_path="/api/v1",
responses(
(status = 200, description = "Proxies a podcast so people can stream podcasts from the remote \
server")),
tag="podcasts"
)]
#[get("/proxy/podcast")]
pub(crate) async fn proxy_podcast(
    mut payload: web::Payload,
    params: web::Query<Params>,
    peer_addr: Option<PeerAddr>,
    query: Option<web::Query<RSSAPiKey>>,
    method: Method,
    rq: HttpRequest,
) -> Result<HttpResponse, Error> {
    let is_auth_enabled =
        is_env_var_present_and_true(BASIC_AUTH) || is_env_var_present_and_true(OIDC_AUTH);

    if is_auth_enabled {
        if query.is_none() {
            return Ok(HttpResponse::Unauthorized().finish());
        }
        let api_key = query.unwrap().0;

        let api_key_exists = User::check_if_api_key_exists(api_key.api_key.to_string());

        if !api_key_exists {
            return Ok(HttpResponse::Unauthorized().body("Unauthorized"));
        }
    }

    let opt_res = PodcastEpisodeService::get_podcast_episode_by_id(&params.episode_id)?;
    if opt_res.clone().is_none() {
        return Ok(HttpResponse::NotFound().finish());
    }
    let episode = opt_res.unwrap();
    let (tx, rx) = mpsc::unbounded_channel();

    actix_web::rt::spawn(async move {
        while let Some(chunk) = payload.next().await {
            tx.send(chunk).unwrap();
        }
    });

    let mut header_map = HeaderMap::new();
    let headers_from_request = rq.headers().clone();
    for (header, header_value) in headers_from_request {
        if header == "host"
            || header == "referer"
            || header == "sec-fetch-site"
            || header == "sec-fetch-mode"
        {
            continue;
        }
        let header = reqwest::header::HeaderName::from_str(header.as_ref()).unwrap();
        header_map.append(header, header_value.to_str().unwrap().parse().unwrap());
    }

    add_basic_auth_headers_conditionally(episode.clone().url, &mut header_map);
    // Required to not generate a 302 redirect
    header_map.append("sec-fetch-mode", "no-cors".parse().unwrap());
    header_map.append("sec-fetch-site", "cross-site".parse().unwrap());
    use std::str::FromStr;
    let forwarded_req = get_http_client()
        .request(
            reqwest::Method::from_str(method.as_str()).unwrap(),
            episode.url,
        )
        .headers(header_map)
        .fetch_mode_no_cors()
        .body(reqwest::Body::wrap_stream(UnboundedReceiverStream::new(rx)));

    let forwarded_req = match peer_addr {
        Some(PeerAddr(addr)) => forwarded_req.header("x-forwarded-for", addr.ip().to_string()),
        None => forwarded_req,
    };

    let res = forwarded_req
        .send()
        .await
        .map_err(error::ErrorInternalServerError)?;

    let mut client_resp =
        HttpResponse::build(actix_web::http::StatusCode::from_u16(res.status().as_u16()).unwrap());
    for (header_name, header_value) in res.headers().iter() {
        client_resp.insert_header((header_name.as_str(), header_value.to_str().unwrap()));
    }

    Ok(client_resp.streaming(res.bytes_stream()))
}

#[put("/podcasts/{id}/settings")]
pub async fn update_podcast_settings(
    id: Path<i32>,
    settings: Json<PodcastSetting>,
    requester: web::ReqData<User>,
) -> Result<HttpResponse, CustomError> {
    if !requester.is_privileged_user() {
        return Err(CustomError::Forbidden);
    }

    let id_num = id.into_inner();
    let mut settings = settings.into_inner();
    settings.podcast_id = id_num;
    let updated_podcast = PodcastSetting::update_settings(&settings)?;

    Ok(HttpResponse::Ok().json(updated_podcast))
}

#[get("/podcasts/{id}/settings")]
pub async fn get_podcast_settings(
    id: Path<i32>,
    requester: web::ReqData<User>,
) -> Result<HttpResponse, CustomError> {
    if !requester.is_privileged_user() {
        return Err(CustomError::Forbidden);
    }

    let id_num = id.into_inner();
    let settings = PodcastSetting::get_settings(id_num)?;

    Ok(HttpResponse::Ok().json(settings))
}

#[post("/podcasts/formatting")]
pub async fn retrieve_podcast_sample_format(
    sample_string: Json<EpisodeFormatDto>,
) -> Result<HttpResponse, CustomError> {
    let podcast = PodcastParsed {
        date: "2021-01-01".to_string(),
        summary: "A podcast about homelabing".to_string(),
        title: "The homelab podcast".to_string(),
        keywords: "computer, server, apps".to_string(),
        language: "en".to_string(),
        explicit: "false".to_string(),
    };
    let settings = Setting {
        id: 0,
        auto_download: false,
        auto_update: false,
        auto_cleanup: false,
        auto_cleanup_days: 0,
        podcast_prefill: 0,
        replace_invalid_characters: false,
        use_existing_filename: false,
        replacement_strategy: "remove".to_string(),
        episode_format: "".to_string(),
        podcast_format: sample_string.0.content,
        direct_paths: true,
    };
    let result = perform_podcast_variable_replacement(settings, podcast, None);

    match result {
        Ok(v) => Ok(HttpResponse::Ok().json(v)),
        Err(e) => Err(CustomError::BadRequest(e.to_string())),
    }
}
