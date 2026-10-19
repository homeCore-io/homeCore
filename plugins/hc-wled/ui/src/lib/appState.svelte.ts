/** `$app/state`'s `page`, as far as the screens use it: where the app is. */
export const page = {
  get url(): URL {
    return new URL(location.href);
  },
};
