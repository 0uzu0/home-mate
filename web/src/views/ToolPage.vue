<script setup lang="ts">
/**
 * Tools page: everything the assistant can DO that is not an external text
 * service — music (household library + Personal Services), weather, household
 * lights (R1j/P0-b: a real state card, no fake form), news (R2-BM6: real
 * feeds only) and the OpenAPI query adapter. Every card is config + probe;
 * none of them adds an interaction rule.
 */
import { t } from '../i18n'
import { advancedMode } from '../stores/advanced'
import MusicCard from '../components/service/MusicCard.vue'
import WeatherCard from '../components/service/WeatherCard.vue'
import HomeCard from '../components/tools/HomeCard.vue'
import NewsCard from '../components/tools/NewsCard.vue'
import SearchCard from '../components/tools/SearchCard.vue'
import OpenApiCard from '../components/tools/OpenApiCard.vue'
import RemoteSocketCard from '../components/tools/RemoteSocketCard.vue'
import OpenSocketsCard from '../components/tools/OpenSocketsCard.vue'
import StoryJokeCard from '../components/tools/StoryJokeCard.vue'
import LearningCard from '../components/tools/LearningCard.vue'
</script>

<template>
  <section class="page">
    <header class="page__head">
      <div>
        <h1 class="page__title">{{ t('toolsPageTitle') }}</h1>
        <p class="page__sub">{{ t('toolsPageSub') }}</p>
      </div>
    </header>

    <div class="page__grid">
      <MusicCard />
      <WeatherCard />
      <HomeCard />
      <NewsCard />
      <SearchCard />
      <StoryJokeCard />
      <LearningCard />
      <OpenApiCard v-if="advancedMode" />
      <RemoteSocketCard v-if="advancedMode" />
      <OpenSocketsCard v-if="advancedMode" />
    </div>
  </section>
</template>

<style scoped>
.page {
  display: flex;
  flex-direction: column;
  gap: var(--sp-4);
}

.page__title {
  font-size: var(--fs-xl);
}

.page__sub {
  margin-top: var(--sp-1);
  color: var(--muted);
  font-size: var(--fs-sm);
  max-width: 80ch;
}

.page__grid {
  display: flex;
  flex-direction: column;
  gap: var(--sp-4);
}
</style>
