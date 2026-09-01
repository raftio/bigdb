"use client";
import * as React from "react";
import { Monitor, Moon, Sun } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Dropdown, DropdownTrigger, DropdownContent, DropdownCheckItem, DropdownLabel } from "@/components/ui/dropdown";

type Mode = "dark" | "light" | "system";

/** Dark-first with a real light theme — both authored, neither derived. */
export function applyTheme(mode: Mode) {
  const dark = mode === "dark" || (mode === "system" && window.matchMedia("(prefers-color-scheme: dark)").matches);
  document.documentElement.classList.toggle("dark", dark);
  try { localStorage.setItem("bigdb-theme", mode); } catch {}
}

export function ThemeToggle() {
  const [mode, setMode] = React.useState<Mode>("dark");

  React.useEffect(() => {
    const saved = (() => { try { return localStorage.getItem("bigdb-theme") as Mode | null; } catch { return null; } })();
    const m = saved ?? "dark";
    setMode(m);
    applyTheme(m);
  }, []);

  const set = (m: Mode) => { setMode(m); applyTheme(m); };
  const Icon = mode === "light" ? Sun : mode === "system" ? Monitor : Moon;

  return (
    <Dropdown>
      <DropdownTrigger asChild>
        <Button variant="ghost" size="icon" aria-label={`Theme: ${mode}`}><Icon aria-hidden /></Button>
      </DropdownTrigger>
      <DropdownContent>
        <DropdownLabel>Theme</DropdownLabel>
        <DropdownCheckItem checked={mode === "dark"} onSelect={() => set("dark")}>Dark</DropdownCheckItem>
        <DropdownCheckItem checked={mode === "light"} onSelect={() => set("light")}>Light</DropdownCheckItem>
        <DropdownCheckItem checked={mode === "system"} onSelect={() => set("system")}>System</DropdownCheckItem>
      </DropdownContent>
    </Dropdown>
  );
}

/** Runs before paint so the first frame is already the right theme. */
export const themeScript = `(function(){try{var m=localStorage.getItem("bigdb-theme")||"dark";var d=m==="dark"||(m==="system"&&matchMedia("(prefers-color-scheme: dark)").matches);document.documentElement.classList.toggle("dark",d)}catch(e){document.documentElement.classList.add("dark")}})()`;
