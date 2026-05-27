ArcVector 명령어

# 인덱스 생성: vcreate <이름> <차원> <메트릭> [인덱스타입]

```
vcreate myidx 3 L2
vcreate myidx 3 COSINE HNSW
```

# 벡터 추가: vadd <이름> <id> [f1,f2,...,fn]

```
vadd myidx vec1 [1.0,2.0,3.0]
vadd myidx vec2 [0.5,1.5,2.5]
```

# 유사 검색: vsearch <이름> <k> [쿼리벡터]

```
vsearch myidx 2 [1.0,2.0,3.0]
```

# 벡터 삭제: vdel <이름> <id>

```
vdel myidx vec1
```

# 인덱스 삭제: vdrop <이름>

```
vdrop myidx
```

# 인덱스 목록: vlist

```
vlist
```

메트릭: L2 (유클리드 거리), COSINE (코사인 유사도), IP (내적)

인덱스 타입: FLAT (기본, 전수 탐색), HNSW (근사 최근접 이웃, 대용량에 적합)