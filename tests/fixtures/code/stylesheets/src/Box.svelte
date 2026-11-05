<script>
  let on = true;
</script>

<div class="tile {on ? 'chip' : 'plain'}">
  <p class:flag class:pill={on}>x</p>
  <p class={on ? 'dot' : undefined}>y</p>
</div>

<style lang="sass">
.plain
  color: gray
</style>
