<script setup lang="ts">
const on = true;
</script>

<template>
  <div class="tile wrapper">
    <span :class="{ chip: on, pill: !on }">a</span>
    <span v-bind:class="[on ? 'dot' : 'ring']">b</span>
    <i :class="$style.local">c</i>
  </div>
</template>

<style scoped lang="scss">
@use "./tokens/tokens" as t;

.wrapper {
  margin: t.$gap;
}

.local {
  color: red;
}
</style>
