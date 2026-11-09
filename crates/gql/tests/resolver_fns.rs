use std::ptr;

use gql::ResolveFn;

#[test]
fn non_send_ctx() {
    async fn point() {}

    struct NonSendCtx {
        ptr: *mut (),
    }

    let y = ResolveFn::PtrAsync(|args, result| {
        Box::pin(async move {
            // TODO: Right now this is also a future lifetime error
            // let ctx = args.context.downcast_ref::<NonSendCtx>().unwrap();
            let ctx = NonSendCtx {
                ptr: ptr::null_mut(),
            };

            // Just to ensure the `!Send` value can be held over the await point
            point().await;
            drop(ctx); // Make sure Rust doesn't drop it before the await point

            // TODO: This right now is a future lifetime error
            // result.serialize_str("Hello World")
        })
    });
}

#[test]
fn forget_to_use_result() {
    // TODO
}
