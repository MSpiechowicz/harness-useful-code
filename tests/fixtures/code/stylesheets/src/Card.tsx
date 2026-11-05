import './card.css';
import { cva } from 'class-variance-authority';
import clsx from 'clsx';
import { cn } from './cn';

const badge = cva('badge', { variants: { tone: { warm: 'badge--warm' } } });

export function Card({ on }: { on: boolean }) {
  const kind = 'note';

  return (
    <div className={cn('card', on && 'card--on')} title={kind}>
      <span className={clsx('title', { tile: on })}>{badge({ tone: 'warm' })}</span>
    </div>
  );
}
