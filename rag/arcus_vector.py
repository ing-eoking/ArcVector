import socket
import struct
import time


class ArcusVector:
    def __init__(self, host='127.0.0.1', port=11211):
        self.host = host
        self.port = port
        self.sock = None
        self._connect()

    def _connect(self):
        self.sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        self.sock.connect((self.host, self.port))
        self.sock.settimeout(10.0)

    def _send(self, cmd):
        try:
            self.sock.sendall((cmd + '\r\n').encode())
        except (OSError, BrokenPipeError):
            self._connect()
            self.sock.sendall((cmd + '\r\n').encode())

    def _recv_line(self):
        buf = b''
        while not buf.endswith(b'\r\n'):
            buf += self.sock.recv(1)
        return buf.decode().strip()

    def _recv_until_end(self):
        buf = b''
        try:
            while True:
                chunk = self.sock.recv(4096)
                if not chunk:
                    break
                buf += chunk
                if buf.endswith(b'END\r\n'):
                    break
                # CLIENT_ERROR / SERVER_ERROR 같은 단일 라인 응답은 END\r\n 없이 끝남
                if buf.endswith(b'\r\n'):
                    last_line = buf.rstrip(b'\r\n').split(b'\r\n')[-1]
                    if (last_line.startswith(b'CLIENT_ERROR') or
                            last_line.startswith(b'SERVER_ERROR') or
                            last_line == b'ERROR'):
                        break
        except socket.timeout:
            pass
        return buf.decode()

    def vcreate(self, index, dim, metric='COSINE', index_type='HNSW', max_elements=100000):
        self._send(f'vcreate {index} {dim} {metric} {index_type} {max_elements}')
        return self._recv_line()

    def vadd(self, index, vec_id, vector, payload=None):
        # 벡터는 little-endian float32 바이너리 blob으로 nread 전송
        blob = struct.pack(f'<{len(vector)}f', *vector)
        if payload:
            data = payload.encode('utf-8')
            self._send(f'vadd {index} {vec_id} {len(blob)} {len(data)}')
            self.sock.sendall(blob + data + b'\r\n')
        else:
            self._send(f'vadd {index} {vec_id} {len(blob)}')
            self.sock.sendall(blob + b'\r\n')
        return self._recv_line()

    def vsearch(self, index, k, vector, threshold=None):
        blob = struct.pack(f'<{len(vector)}f', *vector)
        cmd = f'vsearch {index} {k} {len(blob)}'
        if threshold is not None:
            cmd += f' {threshold}'
        self._send(cmd)
        self.sock.sendall(blob + b'\r\n')
        raw = self._recv_until_end()
        results = []
        lines = raw.split('\r\n')
        i = 0
        while i < len(lines):
            line = lines[i]
            if line == 'END' or line == '':
                break
            if line.startswith('CLIENT_ERROR') or line.startswith('SERVER_ERROR') or line == 'ERROR':
                break
            parts = line.split(' ', 2)
            if len(parts) < 2:
                i += 1
                continue
            vec_id = parts[0]
            score = float(parts[1])
            payload_len = int(parts[2]) if len(parts) > 2 else 0
            i += 1
            payload = lines[i] if payload_len > 0 and i < len(lines) else ''
            results.append({'id': vec_id, 'score': score, 'payload': payload})
            i += 1
        return results

    def vlist(self):
        self._send('vlist')
        return self._recv_until_end()

    def close(self):
        self.sock.close()
