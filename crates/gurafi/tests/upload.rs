use gurafi::{
    Input, NoArgs, Schema, Upload,
    gql::{Object, Request, Value},
};

#[derive(Input)]
struct UploadArgs {
    file: Upload,
}

struct Ctx;

#[test]
fn upload_is_an_input_scalar_and_reaches_the_resolver() {
    let mut schema = Schema::<Ctx>::new();
    schema.query(|q| {
        q.field("ok", |_: NoArgs, _: &Ctx| true);
    });
    schema.mutation(|m| {
        m.field_result("upload", |args: UploadArgs, _: &Ctx| {
            Ok::<_, gql::GraphQLError>(format!(
                "{}:{}:{}",
                args.file.filename().unwrap_or("<unnamed>"),
                args.file
                    .content_type()
                    .unwrap_or("application/octet-stream"),
                String::from_utf8_lossy(args.file.bytes())
            ))
        });
    });

    let schema = schema.build().expect("valid schema");
    let upload = gql::Upload::new("note.txt", "text/plain", b"hello".to_vec());
    let mut variables = Object::new();
    variables.insert("file", Value::external(upload));

    let response = futures::executor::block_on(gql::execute(
        &schema,
        Request::new("mutation($file: Upload!) { upload(file: $file) }").variables(&variables),
        &gql::ExecutionOptions::with_context(Ctx),
    ));

    assert!(response.errors.is_empty(), "{:?}", response.errors);
    assert_eq!(
        response
            .data
            .as_ref()
            .and_then(Value::as_object)
            .and_then(|data| data.get("upload"))
            .and_then(Value::as_str),
        Some("note.txt:text/plain:hello")
    );
}
