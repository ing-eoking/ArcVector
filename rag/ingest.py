"""
사용법:
  python ingest.py              # rag/docs 디렉토리 전체 인제스트
  python ingest.py doc1.txt ... # 파일 직접 지정

환경변수:
  ARCUS_HOST   - ARCUS 호스트 (기본값: 127.0.0.1)
  ARCUS_PORT   - ARCUS 포트 (기본값: 11211)
  INDEX_NAME   - 인덱스 이름 (기본값: docs)
  OLLAMA_HOST  - Ollama 호스트 (기본값: http://localhost:11434)
  DOCS_DIR     - 문서 디렉토리 (기본값: ./docs)
"""

import os
import sys
import hashlib
import requests
from pathlib import Path
from arcus_vector import ArcusVector

ARCUS_HOST    = os.environ.get('ARCUS_HOST', '127.0.0.1')
ARCUS_PORT    = int(os.environ.get('ARCUS_PORT', 11211))
INDEX_NAME    = os.environ.get('INDEX_NAME', 'docs')
OLLAMA_HOST   = os.environ.get('OLLAMA_HOST', 'http://localhost:11434')
DOCS_DIR      = os.environ.get('DOCS_DIR', Path(__file__).parent / 'docs')
DIMENSION     = 1024
CHUNK_SIZE    = 1000
CHUNK_OVERLAP = 100
TEXT_EXTS     = {'.md', '.txt'}


def chunk_text(text):
    chunks = []
    start = 0
    while start < len(text):
        end = min(start + CHUNK_SIZE, len(text))
        chunks.append(text[start:end].strip())
        if end == len(text):
            break
        start += CHUNK_SIZE - CHUNK_OVERLAP
    return [c for c in chunks if c]


def embed(text):
    resp = requests.post(
        f'{OLLAMA_HOST}/api/embed',
        json={'model': 'bge-m3', 'input': text},
    )
    resp.raise_for_status()
    return resp.json()['embeddings'][0]


def ingest_file(path, av):
    with open(path, encoding='utf-8') as f:
        text = f.read()

    chunks = chunk_text(text)
    print(f'{path}: {len(chunks)}개 청크')

    for i, chunk in enumerate(chunks):
        vec_id = hashlib.sha256(f'{path}:{i}'.encode()).hexdigest()[:16]
        vector = embed(chunk)
        result = av.vadd(INDEX_NAME, vec_id, vector, chunk)
        print(f'  [{i+1:3d}/{len(chunks)}] {vec_id}: {result}')


def collect_files():
    if len(sys.argv) > 1:
        return sys.argv[1:]
    docs_dir = Path(DOCS_DIR)
    if not docs_dir.exists():
        print(f'docs 디렉토리가 없습니다: {docs_dir}')
        sys.exit(1)
    files = sorted(p for p in docs_dir.rglob('*') if p.suffix in TEXT_EXTS)
    if not files:
        print(f'텍스트 파일을 찾을 수 없습니다: {docs_dir}')
        sys.exit(1)
    return files


def main():
    av = ArcusVector(ARCUS_HOST, ARCUS_PORT)

    result = av.vcreate(INDEX_NAME, DIMENSION, 'COSINE', 'HNSW', 100000)
    print(f'인덱스: {result}')

    files = collect_files()
    print(f'총 {len(files)}개 파일 인제스트')

    for path in files:
        ingest_file(path, av)

    av.close()
    print('완료')


if __name__ == '__main__':
    main()
