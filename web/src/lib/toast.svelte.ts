// Short messages that fade by themselves, such as "Link copied".
export interface Toast {
  id: number;
  text: string;
}

let next = 1;
export const toasts = $state<{ list: Toast[] }>({ list: [] });

export function toast(text: string, ms = 2600): void {
  const id = next++;
  toasts.list = [...toasts.list, { id, text }];
  setTimeout(() => {
    toasts.list = toasts.list.filter((x) => x.id !== id);
  }, ms);
}
