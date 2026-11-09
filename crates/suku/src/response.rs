// use std::{
//     pin::Pin,
//     task::{Context, Poll},
// };

// pub struct Response<'a> {
//     document: graphql_parser::query::Document<'a, &'a str>,
//     tx_buffer: String,
//     done: bool,
// }

// impl<'a> Response<'a> {
//     pub(crate) fn new(document: graphql_parser::query::Document<'a, &'a str>) -> Self {
//         Self {
//             document,
//             tx_buffer: String::new(),
//             done: false,
//         }
//     }
// }

// // TODO: Wrap this stream in tracing support
// impl<'a> futures::Stream for Response<'a> {
//     type Item = Result<Vec<u8>, Box<dyn std::error::Error + Send + Sync>>;

//     fn poll_next(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
//         if self.done {
//             return Poll::Ready(None);
//         }

//         // resolve(self, &self.schema, &mut response, query);

//         self.done = true;

//         Poll::Ready(Some(Ok("Hello World".to_string().into_bytes().into())))
//     }
// }
