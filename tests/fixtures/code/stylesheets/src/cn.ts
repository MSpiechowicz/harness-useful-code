export function cn(...parts: unknown[]): string {
  return parts.filter(Boolean).join(' ');
}
