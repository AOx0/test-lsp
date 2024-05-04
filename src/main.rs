#![allow(clippy::print_stderr)]
use core::panic;
use indexmap::IndexSet;
use itertools::Itertools;
use logos::Logos;
use lsp_server::{Connection, ExtractError, Message, Request, RequestId, Response};
use lsp_types::notification::{DidChangeTextDocument, DidOpenTextDocument};
use lsp_types::request::Completion;
use lsp_types::{
    CompletionItem, CompletionItemKind, CompletionOptions, CompletionResponse, Position,
    TextDocumentItem, Url, VersionedTextDocumentIdentifier,
};
use lsp_types::{InitializeParams, ServerCapabilities};
use pyo3::types::{IntoPyDict, PyAnyMethods};
use std::collections::HashMap;
use std::error::Error;

#[derive(Logos, Debug, PartialEq, Eq, Clone, Copy)]
enum Token<'s> {
    #[regex(r#"[a-zA-Z_0-9]+"#, |lex| lex.slice())]
    Word(&'s str),
    #[regex(r#"[^a-zA-Z_0-9]"#, |lex| lex.slice())]
    Symbol(&'s str),
}

fn main() -> Result<(), Box<dyn Error + Sync + Send>> {
    // Start logging
    let _ = {
        use log::LevelFilter::*;
        env_logger::builder()
            .filter_module("test-lsp", Info)
            .try_init()
    };

    log::info!("starting generic LSP server");

    //     pyo3::Python::with_gil(|py| -> pyo3::PyResult<()> {
    //         let sys = py.import_bound("sys")?;
    //         let version: String = sys.getattr("version")?.extract()?;

    //         let locals = [
    //             ("os", py.import_bound("os")?),
    //             ("tensorflow", py.import_bound("tensorflow")?),
    //             ("contractions", py.import_bound("contractions")?),
    //         ]
    //         .into_py_dict_bound(py);
    //         let code = r###"
    // a = 4
    // ret = a + 4
    // "###;
    //         py.run_bound(code, None, Some(&locals))?;
    //         let ret: usize = locals.get_item("ret")?.extract()?;

    //         println!("Hello {}, I'm Python {}", ret, version);
    //         Ok(())
    //     })
    //     .unwrap();

    // Create the transport. Includes the stdio (stdin and stdout) versions but this could
    // also be implemented to use sockets or HTTP.
    let (connection, io_threads) = Connection::stdio();

    // Run the server and wait for the two threads to end (typically by trigger LSP Exit event).
    let server_capabilities = serde_json::to_value(ServerCapabilities {
        text_document_sync: Some(lsp_types::TextDocumentSyncCapability::Kind(
            lsp_types::TextDocumentSyncKind::FULL,
        )),
        completion_provider: Some(CompletionOptions {
            trigger_characters: Some(
                [" ", "\t", "\n", "\r"]
                    .into_iter()
                    .map(str::to_string)
                    .collect_vec(),
            ),
            ..Default::default()
        }),
        ..Default::default()
    })
    .unwrap();

    let initialization_params = match connection.initialize(server_capabilities) {
        Ok(it) => it,
        Err(e) => {
            if e.channel_is_disconnected() {
                io_threads.join()?;
            }
            return Err(e.into());
        }
    };
    main_loop(connection, initialization_params)?;
    io_threads.join()?;

    // Shut down gracefully.
    eprintln!("shutting down server");
    Ok(())
}

fn main_loop(
    connection: Connection,
    params: serde_json::Value,
) -> Result<(), Box<dyn Error + Sync + Send>> {
    let _params: InitializeParams = serde_json::from_value(params).unwrap();
    let mut contents: HashMap<Url, String> = HashMap::new();

    for msg in &connection.receiver {
        eprintln!("got msg: {msg:?}");
        match msg {
            Message::Request(req) => {
                if connection.handle_shutdown(&req)? {
                    return Ok(());
                }
                eprintln!("got request: {req:?}");
                match cast_req::<Completion>(req) {
                    Ok((
                        id,
                        lsp_types::CompletionParams {
                            text_document_position,
                            ..
                        },
                    )) => {
                        let position = text_document_position.position;
                        let file = text_document_position.text_document.uri;
                        let Some(words): Option<IndexSet<&str>> = pos_to_words_of_line(
                            position,
                            contents.get(&file).expect("We trust the LSP"),
                            |token| match token {
                                Token::Word(w) => Some(w),
                                Token::Symbol(_) => None,
                            },
                        )
                        .map(|w| w.into_iter().collect()) else {
                            continue;
                        };

                        let result = serde_json::to_value(&Some(CompletionResponse::Array(
                            words
                                .into_iter()
                                .map(|v| CompletionItem {
                                    label: v.to_string(),
                                    kind: Some(CompletionItemKind::TEXT),
                                    documentation: Some(lsp_types::Documentation::String(
                                        "An AI suggested completion".to_string(),
                                    )),
                                    ..Default::default()
                                })
                                .collect_vec(),
                        )))
                        .unwrap();

                        let resp = Response {
                            id,
                            result: Some(result),
                            error: None,
                        };
                        connection.sender.send(Message::Response(resp))?;
                        continue;
                    }
                    Err(err @ ExtractError::JsonError { .. }) => panic!("{err:?}"),
                    Err(ExtractError::MethodMismatch(req)) => req,
                };
            }
            Message::Response(resp) => {
                eprintln!("got response: {resp:?}");
            }
            Message::Notification(not) => {
                eprintln!("got notification: {not:?}");
                match cast_not::<DidOpenTextDocument>(not.clone()) {
                    Ok(lsp_types::DidOpenTextDocumentParams {
                        text_document: TextDocumentItem { uri, text, .. },
                    }) => {
                        eprintln!("{uri} :: {text:?}");
                        contents.insert(uri, text);
                        continue;
                    }
                    Err(err @ ExtractError::JsonError { .. }) => panic!("{err:?}"),
                    Err(ExtractError::MethodMismatch(not)) => not,
                };
                match cast_not::<DidChangeTextDocument>(not) {
                    Ok(lsp_types::DidChangeTextDocumentParams {
                        text_document: VersionedTextDocumentIdentifier { uri, .. },
                        content_changes,
                    }) => {
                        let text = content_changes.first().unwrap().text.to_string();
                        eprintln!("{uri} :: {text:?}");
                        contents.insert(uri, text);
                        continue;
                    }
                    Err(err @ ExtractError::JsonError { .. }) => panic!("{err:?}"),
                    Err(ExtractError::MethodMismatch(not)) => not,
                };
            }
        }
    }
    Ok(())
}

fn pos_to_words_of_line(
    Position { line, character }: Position,
    text: &str,
    mut filter: impl for<'s> FnMut(Token<'s>) -> Option<&'s str>,
) -> Option<Vec<&str>> {
    text.lines()
        .nth(line.try_into().unwrap())
        .map(|s| &s[..character.try_into().unwrap()])
        .map(|context| {
            Token::lexer(context)
                .filter_map(|a| match a {
                    Ok(v) => filter(v),
                    Err(_) => None,
                })
                .collect()
        })
}

fn cast_req<R>(req: Request) -> Result<(RequestId, R::Params), ExtractError<Request>>
where
    R: lsp_types::request::Request,
    R::Params: serde::de::DeserializeOwned,
{
    req.extract(R::METHOD)
}

fn cast_not<N>(
    not: lsp_server::Notification,
) -> Result<N::Params, ExtractError<lsp_server::Notification>>
where
    N: lsp_types::notification::Notification,
    N::Params: serde::de::DeserializeOwned,
{
    not.extract(N::METHOD)
}
