export function register(on) {
  const remove = on("session.append", async ($, event, next) => await next(event));
  return () => remove();
}
