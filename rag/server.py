"""
사용법:
  python server.py

환경변수:
  ARCUS_HOST   - ARCUS 호스트 (기본값: 127.0.0.1)
  ARCUS_PORT   - ARCUS 포트 (기본값: 11211)
  INDEX_NAME   - 인덱스 이름 (기본값: docs)
  PORT         - 서버 포트 (기본값: 8000)
  TOP_K        - 검색 결과 수 (기본값: 5)
  OLLAMA_HOST  - Ollama 호스트 (기본값: http://localhost:11434)
  EMBED_MODEL  - 임베딩 모델 (기본값: bge-m3)
  GEN_MODEL    - 생성 모델 (기본값: llama3.2:latest, exaone3.5 크래시로 인한 임시값)
"""

import os
import requests
from flask import Flask, request, jsonify
from flask_cors import CORS
from arcus_vector import ArcusVector

ARCUS_HOST       = os.environ.get('ARCUS_HOST', '127.0.0.1')
ARCUS_PORT       = int(os.environ.get('ARCUS_PORT', 11211))
INDEX_NAME       = os.environ.get('INDEX_NAME', 'docs')
PORT             = int(os.environ.get('PORT', 8000))
TOP_K            = int(os.environ.get('TOP_K', 5))
OLLAMA_HOST      = os.environ.get('OLLAMA_HOST', 'http://localhost:11434')
SCORE_THRESHOLD  = float(os.environ.get('SCORE_THRESHOLD', 0.5))
EMBED_MODEL      = os.environ.get('EMBED_MODEL', 'bge-m3')
# 임시 우회: Ollama 0.24.0이 exaone3.5 러너를 크래시시켜 llama3.2로 대체한다.
#           Ollama 업데이트 후 'exaone3.5:2.4b'로 되돌릴 것.
GEN_MODEL        = os.environ.get('GEN_MODEL', 'llama3.2:latest')

app = Flask(__name__)
app.json.ensure_ascii = False
CORS(app)

@app.errorhandler(Exception)
def handle_error(e):
    return jsonify({'error': str(e)}), 500

av = ArcusVector(ARCUS_HOST, ARCUS_PORT)


def _ollama_post(path, payload):
    resp = requests.post(f'{OLLAMA_HOST}{path}', json=payload)
    if not resp.ok:
        # Ollama가 보낸 실제 에러 메시지를 그대로 노출한다.
        # (raise_for_status는 본문을 버려 "model runner has unexpectedly stopped" 같은 원인을 숨긴다)
        try:
            detail = resp.json().get('error', resp.text)
        except ValueError:
            detail = resp.text
        raise RuntimeError(f'Ollama {path} 실패 (HTTP {resp.status_code}): {detail}')
    return resp.json()


def embed(text):
    return _ollama_post('/api/embed', {'model': EMBED_MODEL, 'input': text})['embeddings'][0]


def generate(prompt):
    return _ollama_post(
        '/api/generate',
        {'model': GEN_MODEL, 'prompt': prompt, 'stream': False},
    )['response']


def build_prompt(query, chunks):
    context = '\n\n'.join(
        f'[{i+1}] {c["payload"]}'
        for i, c in enumerate(chunks)
        if c['payload']
    )
    return f"""You are a Korean-language assistant. Answer ONLY in Korean. Use ONLY the provided documents.

Rules:
- Answer in Korean only. Never mix other languages.
- Use only information from the documents below.
- If the question is unrelated to the documents, reply: "문서에서 관련 내용을 찾을 수 없습니다."
- Be concise. No introduction, no preamble.

Documents:
{context}

Question: {query}

Answer in Korean:"""


@app.post('/chat')
def chat():
    body = request.get_json(force=True)
    query = body.get('message', '').strip()
    if not query:
        return jsonify({'error': 'message is required'}), 400

    GREETINGS = {'안녕', '안녕하세요', '하이', 'hi', 'hello', '반가워', '반갑습니다'}
    if query.strip().rstrip('!?~.').lower() in GREETINGS:
        return jsonify({'answer': '안녕하세요! Arcus 문서에 대해 궁금한 점을 질문해 주세요.', 'sources': []})

    vector = embed(query)
    all_chunks = av.vsearch(INDEX_NAME, TOP_K, vector)
    print(f'[debug] scores: {[round(c["score"],4) for c in all_chunks]}')
    chunks = [c for c in all_chunks if c['score'] <= SCORE_THRESHOLD]

    if not chunks:
        return jsonify({
            'answer': '문서에서 관련 내용을 찾을 수 없습니다.',
            'sources': [],
            'debug_scores': [round(c['score'], 4) for c in all_chunks],
        })

    prompt = build_prompt(query, chunks)
    answer = generate(prompt)

    return jsonify({
        'answer': answer,
        'sources': [{'id': c['id'], 'score': c['score'], 'text': c['payload']} for c in chunks],
    })


@app.get('/health')
def health():
    return jsonify({'status': 'ok', 'index': av.vlist().strip()})


if __name__ == '__main__':
    print(f'서버 시작: http://localhost:{PORT}')
    app.run(host='0.0.0.0', port=PORT, debug=False)
