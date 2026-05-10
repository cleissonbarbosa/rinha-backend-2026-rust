use tokio_uring::buf::BoundedBuf;

fn main() {
    let buf = Vec::with_capacity(1024);
    let slice = buf.slice(10..);
    let buf_recovered = slice.into_inner();
}
