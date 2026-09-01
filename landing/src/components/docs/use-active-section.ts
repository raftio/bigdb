import * as React from "react";

/**
 * Which section the reader is in, by id. Read off scroll position rather than an
 * IntersectionObserver: a long band and a short one behave the same way this way — the
 * active entry is the last heading the reader has passed, which is what a table of
 * contents is claiming when it highlights one.
 */
export function useActiveSection(ids: string[]) {
  const [active, setActive] = React.useState(ids[0] ?? "");

  React.useEffect(() => {
    if (!ids.length) return;

    const pick = () => {
      /* Clear the sticky header, then a little more: a heading right under it reads as
         "the one I am on", not "the one I am about to leave". */
      const line = window.scrollY + 140;
      let current = ids[0];
      for (const id of ids) {
        const el = document.getElementById(id);
        if (el && el.getBoundingClientRect().top + window.scrollY <= line) current = id;
      }
      /* At the very bottom nothing below can win, so hand the last one over. */
      if (window.innerHeight + window.scrollY >= document.body.scrollHeight - 8) {
        current = ids[ids.length - 1];
      }
      setActive(current);
    };

    pick();
    window.addEventListener("scroll", pick, { passive: true });
    window.addEventListener("resize", pick);
    return () => {
      window.removeEventListener("scroll", pick);
      window.removeEventListener("resize", pick);
    };
  }, [ids.join("|")]);

  return active;
}
