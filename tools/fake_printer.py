#!/usr/bin/env python3
"""Fake Zebra network printer for testing zprint without hardware.

Listens on port 9200 (default; 9100 is where zprint answers Zebra BrowserPrint), prints whatever it receives, and answers
~HS with a host status reply.

    python3 tools/fake_printer.py [--port 9200] [--paper-out] [--head-open]
"""

import argparse
import socketserver

STX, ETX = "\x02", "\x03"


def host_status(paper_out: bool, head_open: bool) -> bytes:
    s1 = f"030,{int(paper_out)},0,1245,000,0,0,0,000,0,0,0"
    s2 = f"001,0,{int(head_open)},0,1,2,6,0,00000000,1,000"
    s3 = "1234,0"
    return "".join(f"{STX}{s}{ETX}\r\n" for s in (s1, s2, s3)).encode()


class Handler(socketserver.BaseRequestHandler):
    def handle(self):
        self.request.settimeout(5)
        data = b""
        answered = False
        try:
            while True:
                chunk = self.request.recv(4096)
                if not chunk:
                    break
                data += chunk
                if b"~HS" in data and not answered:
                    self.request.sendall(host_status(ARGS.paper_out, ARGS.head_open))
                    answered = True
        except TimeoutError:
            pass
        print(f"--- {len(data)} bytes from {self.client_address[0]} ---")
        print(data.decode(errors="replace"))


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--port", type=int, default=9200)
    parser.add_argument("--paper-out", action="store_true")
    parser.add_argument("--head-open", action="store_true")
    ARGS = parser.parse_args()

    socketserver.ThreadingTCPServer.allow_reuse_address = True
    with socketserver.ThreadingTCPServer(("127.0.0.1", ARGS.port), Handler) as server:
        print(f"fake printer listening on 127.0.0.1:{ARGS.port}")
        server.serve_forever()
