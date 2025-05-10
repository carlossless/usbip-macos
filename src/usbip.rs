use std::io::Error;

use bytes::{BufMut, BytesMut};
use tokio::{io::{AsyncReadExt, AsyncWriteExt}, net::TcpStream};

pub async fn connect_to_usbip_server(addr: &str) -> Result<(), Error> {
    let mut stream = TcpStream::connect(addr).await?;
    println!("Connected to USBIP server at {}", addr);

    // Example: send USBIP_REQ_DEVLIST (0x8005)
    let mut request = BytesMut::with_capacity(48);
    request.put_u16(273);    // version
    request.put_u16(0x8005); // command code
    request.put_u32(0);      // status
    request.resize(48, 0);   // pad to header size

    stream.write_all(&request).await?;
    println!("Sent USBIP_DEVLIST request");

    let mut buf = BytesMut::with_capacity(1024);
    stream.read_buf(&mut buf).await?;

    if buf.len() < 8 {
        return Err(Error::new(std::io::ErrorKind::UnexpectedEof, "Incomplete response header"));
    }

    let payload = &buf[0..];
    println!("Response payload: {:?}", payload);

    Ok(())
}
