use std::io;
use stream;


quick_error! {
    #[derive(Debug)]
    pub enum DecodeError {
        Io(e: io::Error) {
            display("i/o error: {}", e)
            cause(e)
            from()
        }
        Type(t: u8) {
            display("unkown type: {}", t)
        }
        #[doc(hidden)]
        __Nonexhaustive
    }
}


quick_error! {
    #[derive(Debug)]
    pub enum StreamError {
        StreamClosed(id: stream::Id) {
            display("stream {} is closed", id)
        }
        ConnectionClosed {
            display("connection of this stream is closed")
        }
        BodyTooLarge {
            display("body size exceeds allowed maximum")
        }
        #[doc(hidden)]
        __Nonexhaustive
    }
}


quick_error! {
    #[derive(Debug)]
    pub enum ConnectionError {
        Io(e: io::Error) {
            display("i/o error: {}", e)
            cause(e)
            from()
        }
        Decode(e: DecodeError) {
            display("decode error: {}", e)
            cause(e)
            from()
        }
        Protocol(error_code: u32) {
            display("protocol error {}", error_code)
        }
        NoMoreStreamIds {
            display("number of stream ids has been exhausted")
        }
        Closed {
            display("connection is closed")
        }
        #[doc(hidden)]
        __Nonexhaustive
    }
}
