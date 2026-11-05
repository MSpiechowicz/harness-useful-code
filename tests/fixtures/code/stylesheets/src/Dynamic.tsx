import './dynamic.css';

const base = 'dyn-base';
const unused = 'dyn-unused';
const shadow = 'dyn-shadow';

export function Dynamic({ shadow }: { shadow: string }) {
  const note = unused.length;

  document.body.classList.toggle('dyn-toggle', 'dyn-not-this');
  document.querySelector('.dyn-query > .dyn-child:not(.dyn-excluded)');

  return <Wrap wrapperClassName="dyn-wrap" className={base} title={shadow} data-n={note} />;
}
