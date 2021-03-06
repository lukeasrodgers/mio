use mio::*;
use mio::net::*;
use mio::net::pipe::*;
use mio::buf::{ByteBuf, MutByteBuf, SliceBuf};
use mio::util::Slab;
use std::old_io::TempDir;

type TestEventLoop = EventLoop<usize, ()>;

const SERVER: Token = Token(0);
const CLIENT: Token = Token(1);

struct EchoConn {
    sock: UnixSocket,
    buf: Option<ByteBuf>,
    mut_buf: Option<MutByteBuf>,
    token: Token,
    interest: Interest,
}

impl EchoConn {
    fn new(sock: UnixSocket) -> EchoConn {
        EchoConn {
            sock: sock,
            buf: None,
            mut_buf: Some(ByteBuf::mut_with_capacity(2048)),
            token: Token(-1),
            interest: Interest::hup(),
        }
    }

    fn writable(&mut self, event_loop: &mut TestEventLoop) -> MioResult<()> {
        let mut buf = self.buf.take().unwrap();

        match self.sock.write(&mut buf) {
            Ok(NonBlock::WouldBlock) => {
                debug!("client flushing buf; WOULDBLOCK");

                self.buf = Some(buf);
                self.interest.insert(Interest::writable());
            }
            Ok(NonBlock::Ready(r)) => {
                debug!("CONN : we wrote {} bytes!", r);

                self.mut_buf = Some(buf.flip());
                self.interest.insert(Interest::readable());
                self.interest.remove(Interest::writable());
            }
            Err(e) => debug!("not implemented; client err={:?}", e),
        }

        event_loop.reregister(&self.sock, self.token, self.interest, PollOpt::edge() | PollOpt::oneshot())
    }

    fn readable(&mut self, event_loop: &mut TestEventLoop) -> MioResult<()> {
        let mut buf = self.mut_buf.take().unwrap();

        match self.sock.read(&mut buf) {
            Ok(NonBlock::WouldBlock) => {
                panic!("We just got readable, but were unable to read from the socket?");
            }
            Ok(NonBlock::Ready(r)) => {
                debug!("CONN : we read {} bytes!", r);
                self.interest.remove(Interest::readable());
                self.interest.insert(Interest::writable());
            }
            Err(e) => {
                debug!("not implemented; client err={:?}", e);
                self.interest.remove(Interest::readable());
            }

        };

        // prepare to provide this to writable
        self.buf = Some(buf.flip());

        event_loop.reregister(&self.sock, self.token, self.interest, PollOpt::edge() | PollOpt::oneshot())
    }
}

struct EchoServer {
    sock: UnixAcceptor,
    conns: Slab<EchoConn>
}

impl EchoServer {
    fn accept(&mut self, event_loop: &mut TestEventLoop) -> MioResult<()> {
        debug!("server accepting socket");

        let sock = self.sock.accept().unwrap().unwrap();
        let conn = EchoConn::new(sock,);
        let tok = self.conns.insert(conn)
            .ok().expect("could not add connectiont o slab");

        // Register the connection
        self.conns[tok].token = tok;
        event_loop.register_opt(&self.conns[tok].sock, tok, Interest::readable(), PollOpt::edge() | PollOpt::oneshot())
            .ok().expect("could not register socket with event loop");

        Ok(())
    }

    fn conn_readable(&mut self, event_loop: &mut TestEventLoop, tok: Token) -> MioResult<()> {
        debug!("server conn readable; tok={:?}", tok);
        self.conn(tok).readable(event_loop)
    }

    fn conn_writable(&mut self, event_loop: &mut TestEventLoop, tok: Token) -> MioResult<()> {
        debug!("server conn writable; tok={:?}", tok);
        self.conn(tok).writable(event_loop)
    }

    fn conn<'a>(&'a mut self, tok: Token) -> &'a mut EchoConn {
        &mut self.conns[tok]
    }
}

struct EchoClient {
    sock: UnixSocket,
    msgs: Vec<&'static str>,
    tx: SliceBuf<'static>,
    rx: SliceBuf<'static>,
    mut_buf: Option<MutByteBuf>,
    token: Token,
    interest: Interest,
}


// Sends a message and expects to receive the same exact message, one at a time
impl EchoClient {
    fn new(sock: UnixSocket, tok: Token,  mut msgs: Vec<&'static str>) -> EchoClient {
        let curr = msgs.remove(0);

        EchoClient {
            sock: sock,
            msgs: msgs,
            tx: SliceBuf::wrap(curr.as_bytes()),
            rx: SliceBuf::wrap(curr.as_bytes()),
            mut_buf: Some(ByteBuf::mut_with_capacity(2048)),
            token: tok,
            interest: Interest::none(),
        }
    }

    fn readable(&mut self, event_loop: &mut TestEventLoop) -> MioResult<()> {
        debug!("client socket readable");

        let mut buf = self.mut_buf.take().unwrap();

        match self.sock.read(&mut buf) {
            Ok(NonBlock::WouldBlock) => {
                panic!("We just got readable, but were unable to read from the socket?");
            }
            Ok(NonBlock::Ready(r)) => {
                debug!("CLIENT : We read {} bytes!", r);
            }
            Err(e) => {
                panic!("not implemented; client err={:?}", e);
            }
        };

        // prepare for reading
        let mut buf = buf.flip();

        debug!("CLIENT : buf = {:?} -- rx = {:?}", buf.bytes(), self.rx.bytes());
        while buf.has_remaining() {
            let actual = buf.read_byte().unwrap();
            let expect = self.rx.read_byte().unwrap();

            assert!(actual == expect, "actual={}; expect={}", actual, expect);
        }

        self.mut_buf = Some(buf.flip());

        self.interest.remove(Interest::readable());

        if !self.rx.has_remaining() {
            self.next_msg(event_loop).unwrap();
        }

        event_loop.reregister(&self.sock, self.token, self.interest, PollOpt::edge() | PollOpt::oneshot())
    }

    fn writable(&mut self, event_loop: &mut TestEventLoop) -> MioResult<()> {
        debug!("client socket writable");

        match self.sock.write(&mut self.tx) {
            Ok(NonBlock::WouldBlock) => {
                debug!("client flushing buf; WOULDBLOCK");
                self.interest.insert(Interest::writable());
            }
            Ok(NonBlock::Ready(r)) => {
                debug!("CLIENT : we wrote {} bytes!", r);
                self.interest.insert(Interest::readable());
                self.interest.remove(Interest::writable());
            }
            Err(e) => debug!("not implemented; client err={:?}", e)
        }

        event_loop.reregister(&self.sock, self.token, self.interest, PollOpt::edge() | PollOpt::oneshot())
    }

    fn next_msg(&mut self, event_loop: &mut TestEventLoop) -> MioResult<()> {
        if self.msgs.is_empty() {
            event_loop.shutdown();
            return Ok(());
        }

        let curr = self.msgs.remove(0);

        debug!("client prepping next message");
        self.tx = SliceBuf::wrap(curr.as_bytes());
        self.rx = SliceBuf::wrap(curr.as_bytes());

        self.interest.insert(Interest::writable());
        event_loop.reregister(&self.sock, self.token, self.interest, PollOpt::edge() | PollOpt::oneshot())
    }
}

struct EchoHandler {
    server: EchoServer,
    client: EchoClient,
}

impl EchoHandler {
    fn new(srv: UnixAcceptor, client: UnixSocket, msgs: Vec<&'static str>) -> EchoHandler {
        EchoHandler {
            server: EchoServer {
                sock: srv,
                conns: Slab::new_starting_at(Token(2), 128)
            },
            client: EchoClient::new(client, CLIENT, msgs)
        }
    }
}

impl Handler<usize, ()> for EchoHandler {
    fn readable(&mut self, event_loop: &mut TestEventLoop, token: Token, hint: ReadHint) {
        assert!(hint.is_data());

        match token {
            SERVER => self.server.accept(event_loop).unwrap(),
            CLIENT => self.client.readable(event_loop).unwrap(),
            i => self.server.conn_readable(event_loop, i).unwrap()
        };
    }

    fn writable(&mut self, event_loop: &mut TestEventLoop, token: Token) {
        match token {
            SERVER => panic!("received writable for token 0"),
            CLIENT => self.client.writable(event_loop).unwrap(),
            _ => self.server.conn_writable(event_loop, token).unwrap()
        };
    }
}

#[test]
pub fn test_unix_echo_server() {
    debug!("Starting TEST_UNIX_ECHO_SERVER");
    let mut event_loop = EventLoop::new().unwrap();

    let tmp_dir = TempDir::new("test_unix_echo_server").unwrap();
    let tmp_sock_path = tmp_dir.path().join(Path::new("sock"));
    let addr = SockAddr::from_path(tmp_sock_path);

    let srv = UnixSocket::stream().unwrap();

    let srv = srv.bind(&addr).unwrap()
        .listen(256).unwrap();

    info!("listen for connections");
    event_loop.register_opt(&srv, SERVER, Interest::readable(), PollOpt::edge() | PollOpt::oneshot()).unwrap();

    let sock = UnixSocket::stream().unwrap();

    // Connect to the server
    event_loop.register_opt(&sock, CLIENT, Interest::writable(), PollOpt::edge() | PollOpt::oneshot()).unwrap();
    sock.connect(&addr).unwrap();

    // Start the event loop
    event_loop.run(EchoHandler::new(srv, sock, vec!["foo", "bar"]))
        .ok().expect("failed to execute event loop");

}
