<!--
  A screenshot of the web UI, captured by ui/e2e/screenshots.mjs in both
  themes at twice the CSS size: the `2x` srcset shows it at its natural size
  on the page and sharp on high-density screens.

    <Screenshot name="rollout-pipeline" alt="The rollout pipeline" />
-->
<template>
    <figure class="screenshot">
        <img v-for="theme in themes" :key="theme" :class="`screenshot__${theme}`"
            :src="src(theme)" :srcset="`${src(theme)} 2x`" :alt="alt" />
        <figcaption v-if="$slots.default"><slot /></figcaption>
    </figure>
</template>

<script setup lang="ts">
import { withBase } from "vuepress/client";

const props = defineProps<{ name: string; alt: string }>();
const themes = ["light", "dark"];
const src = (theme: string) => withBase(`/screenshots/${props.name}.${theme}.png`);
</script>
