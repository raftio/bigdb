import * as React from "react";
import { Moon, Sun } from "lucide-react";
import { Button } from "@/components/ui/button";

type Mode = "dark" | "light";
const KEY = "bigdb-theme";

/**
 * Dark-first with a real light theme — both authored as token sets, neither
 * derived from the other. The choice is remembered, and it is the same key the
 * console uses, so moving between the two does not flip the lights.
 */
export function applyTheme(mode: Mode) {
  document.documentElement.classList.toggle("dark", mode === "dark");
  try { localStorage.setItem(KEY, mode); } catch { /* private mode */ }
}

function initial(): Mode {
  try {
    const saved = localStorage.getItem(KEY);
    if (saved === "light" || saved === "dark") return saved;
  } catch { /* private mode */ }
  return window.matchMedia?.("(prefers-color-scheme: light)").matches ? "light" : "dark";
}

export function ThemeToggle() {
  const [mode, setMode] = React.useState<Mode>("dark");

  React.useEffect(() => {
    const m = initial();
    setMode(m);
    applyTheme(m);
  }, []);

  const next = mode === "dark" ? "light" : "dark";
  const Icon = mode === "dark" ? Moon : Sun;

  return (
    <Button
      variant="ghost"
      size="icon"
      aria-label={`Switch to ${next} theme`}
      onClick={() => { setMode(next); applyTheme(next); }}
    >
      <Icon aria-hidden />
    </Button>
  );
}

/** Runs before paint so the first frame is already the right theme. */
export const themeScript = `(function(){try{var m=localStorage.getItem("bigdb-theme");if(m!=="light"&&m!=="dark"){m=matchMedia("(prefers-color-scheme: light)").matches?"light":"dark"}document.documentElement.classList.toggle("dark",m==="dark")}catch(e){document.documentElement.classList.add("dark")}})()`;
