#[tokio::main]
async fn main() {
    byos::start(std::env::args()).await;
}
