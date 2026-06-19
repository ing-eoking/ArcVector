---
layout: doc
---

<script setup>
import { onMounted } from 'vue'
import { useRouter } from 'vitepress'

const router = useRouter()

onMounted(() => {
  // VitePress router to go to the first page of overview
  router.go('/docs/arcus-memcached@test/0-ARCUS 소개/00-제품 개요/')
})
</script>

<h1 class="text-5xl font-bold text-center text-blue-600 mb-4">ARCUS Memcached @test</h1>

<div class="text-center text-lg text-gray-600">
  리다이렉트 중... <a href="/docs/arcus-memcached@test/0-ARCUS 소개/00-제품 개요/" class="text-blue-500 underline">개요 페이지로 바로 이동</a>
</div>
