extern crate anyhow;
extern crate env_logger;
#[macro_use]
extern crate log;
extern crate tokio;
extern crate tokio_rustls;
#[macro_use]
extern crate lazy_static;
extern crate serde_derive;

use anyhow::{anyhow, Result};

use tokio::io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::stream::StreamExt;

use tokio_rustls::rustls::internal::pemfile::{certs, rsa_private_keys};
use tokio_rustls::rustls::{Certificate, NoClientAuth, PrivateKey, ServerConfig};
use tokio_rustls::TlsAcceptor;

use url::Url;

use std::collections::HashMap;

fn load_certs(file: &str) -> Result<Vec<Certificate>> {
    certs(&mut std::io::BufReader::new(std::fs::File::open(file)?))
        .map_err(|_| anyhow!("invalid certificate"))
}

fn load_keys(file: &str) -> Result<PrivateKey> {
    Ok(
        rsa_private_keys(&mut std::io::BufReader::new(std::fs::File::open(file)?))
            .map_err(|_| anyhow!("invalid key"))?
            .remove(0),
    )
}

#[tokio::main]
async fn main() {
    env_logger::init();
    info!("graffiti is starting up");

    run().await.unwrap();
}

async fn run() -> Result<()> {
    let mut config = ServerConfig::new(NoClientAuth::new());
    config.set_single_cert(load_certs("cert.pem")?, load_keys("key.pem")?)?;
    let acceptor = TlsAcceptor::from(std::sync::Arc::new(config));

    let mut listener = tokio::net::TcpListener::bind("0.0.0.0:1965").await?;

    let server = async {
        let mut incoming = listener.incoming();

        while let Some(conn) = incoming.next().await {
            match conn {
                Ok(stream) => {
                    tokio::spawn(handler(acceptor.clone(), stream));
                }
                Err(err) => error!("connection error: {}", err),
            }
        }
    };

    server.await;
    Ok(())
}

async fn handler(acceptor: TlsAcceptor, stream: TcpStream) -> Result<()> {
    info!("new incoming connection");
    let stream = acceptor.accept(stream).await?;
    let (reader, mut writer) = tokio::io::split(stream);

    let line = BufReader::new(reader).lines().next_line().await?;

    match line {
        Some(contents) => process_incoming(&mut writer, contents).await,
        None => anyhow::bail!("empty incoming line"),
    }
}

// I would've done this as a macro, but it didn't work (rust-lang/rust#64960)
fn response(status: ResponseCode, response_type: &str, body: &str) -> Vec<u8> {
    format!("{} {}\r\n{}", status as u8, response_type, body)
        .as_bytes()
        .to_vec()
}

enum ResponseCode {
    MoreInfo = 10,
    Success = 20,
    CgiError = 42,
    NotFound = 51,
}

async fn process_incoming<T>(writer: &mut T, line: String) -> Result<()>
where
    T: AsyncWrite + std::marker::Unpin,
{
    let url = Url::parse(&line)?;
    info!("request to {}", url.path());

    if url.path() == "/" || url.path() == "" {
        writer
            .write_all(&response(
                ResponseCode::Success,
                "text/gemini",
                include_str!("index.gemini"),
            ))
            .await?;
        return Ok(());
    }

    lazy_static! {
        static ref SUPPORTED_WIKIS: HashMap<&'static str, Url> = {
            let mut m = HashMap::new();
            m.insert(
                "/wikipedia",
                Url::parse("https://en.wikipedia.org/w/api.php").unwrap(),
            );
            m.insert("/nethack", Url::parse("https://nethackwiki.com/w/api.php").unwrap());
            m.insert("/xkcd", Url::parse("https://explainxkcd.com/wiki/api.php").unwrap());
            m
        };
    }

    for (wiki, wiki_url) in SUPPORTED_WIKIS.iter() {
        if &url.path() == wiki {
            match url.query() {
                Some(q) => {
                    let to_send = &wiki_response(q, wiki_url).await;

                    match to_send {
                        Ok(response) => writer.write_all(response).await?,
                        Err(err) => {
                            error!("failed to give response ({})", err);
                            writer
                                .write_all(&response(
                                    ResponseCode::CgiError,
                                    "Failed to create a response :(",
                                    "",
                                ))
                                .await?
                        }
                    }
                }
                None => {
                    writer
                        .write_all(&response(ResponseCode::MoreInfo, "Page name", ""))
                        .await?
                }
            }

            return Ok(());
        }
    }

    warn!("file not found");
    writer
        .write_all(&response(ResponseCode::NotFound, "text/gemini", ""))
        .await?;

    Ok(())
}

#[derive(serde_derive::Deserialize)]
struct RootMediaWikiResponse {
    parse: MediaWikiResponse,
}

#[derive(serde_derive::Deserialize)]
struct MediaWikiResponse {
    title: String,
    pageid: usize,
    wikitext: String,
}

async fn wiki_response(page_name: &str, root_url: &Url) -> Result<Vec<u8>> {
    let mut url = root_url.clone();
    url.set_query(Some(&format!(
        "action=parse&prop=wikitext&formatversion=2&format=json&page={}",
        page_name
    )));

    info!("fetching {}", url.to_string());
    let page = reqwest::get(&url.to_string())
        .await?
        .json::<RootMediaWikiResponse>()
        .await?
        .parse;
    info!("fetching complete");

    Ok(response(
        ResponseCode::Success,
        "text/gemini",
        &format!(
            "# {}\r\nprovided by Graffiti (page id {}@{})\r\n```\r\n{}\r\n```\r\n",
            page.title, page.pageid, url.host().unwrap(), page.wikitext
        ),
    ))
}
