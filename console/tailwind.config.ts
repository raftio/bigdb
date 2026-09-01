import type { Config } from "tailwindcss";

const hsl = (v: string) => `hsl(var(${v}) / <alpha-value>)`;

export default {
  darkMode: "class",
  content: ["./src/**/*.{ts,tsx}"],
  theme: {
    extend: {
      colors: {
        bg: hsl("--bg"),
        surface: { DEFAULT: hsl("--surface"), raised: hsl("--surface-raised"), sunken: hsl("--surface-sunken") },
        overlay: hsl("--overlay"),
        fg: { DEFAULT: hsl("--fg"), muted: hsl("--fg-muted"), faint: hsl("--fg-faint"), inverted: hsl("--fg-inverted") },
        line: { DEFAULT: hsl("--border"), strong: hsl("--border-strong"), grid: hsl("--grid") },
        accent: { DEFAULT: hsl("--accent"), fg: hsl("--accent-fg"), soft: hsl("--accent-soft"), line: hsl("--accent-line") },
        // The accent's opposite number: a bit that is not set.
        unset: hsl("--unset"),
        healthy: { DEFAULT: hsl("--healthy"), soft: hsl("--healthy-soft") },
        degraded: { DEFAULT: hsl("--degraded"), soft: hsl("--degraded-soft") },
        refused: { DEFAULT: hsl("--refused"), soft: hsl("--refused-soft") },
        failed: { DEFAULT: hsl("--failed"), soft: hsl("--failed-soft") },
        unknown: { DEFAULT: hsl("--unknown"), soft: hsl("--unknown-soft") },
        viz: { p50: hsl("--viz-p50"), p95: hsl("--viz-p95"), p99: hsl("--viz-p99"), fill: hsl("--viz-fill") },
      },
      borderRadius: { sm: "var(--radius-sm)", DEFAULT: "var(--radius)", md: "var(--radius)", lg: "var(--radius-lg)" },
      maxWidth: { site: "1180px" },
      boxShadow: { e1: "var(--elev-1)", e2: "var(--elev-2)", e3: "var(--elev-3)" },
      fontFamily: { sans: "var(--font-sans)", mono: "var(--font-mono)" },
      fontSize: {
        "2xs": ["10px", { lineHeight: "14px", letterSpacing: "0.04em" }],
        xs:    ["11px", { lineHeight: "16px" }],
        sm:    ["12px", { lineHeight: "18px" }],
        base:  ["13px", { lineHeight: "20px" }],
        md:    ["14px", { lineHeight: "22px" }],
        lg:    ["16px", { lineHeight: "24px" }],
        xl:    ["20px", { lineHeight: "28px", letterSpacing: "-0.01em" }],
        "2xl": ["26px", { lineHeight: "32px", letterSpacing: "-0.02em" }],
        "3xl": ["34px", { lineHeight: "40px", letterSpacing: "-0.025em" }],
      },
      spacing: { 4.5: "18px", 13: "52px", 15: "60px" },
      transitionDuration: { DEFAULT: "120ms", fast: "80ms" },
      keyframes: {
        "fade-in": { from: { opacity: "0" }, to: { opacity: "1" } },
        "slide-up": { from: { opacity: "0", transform: "translateY(4px)" }, to: { opacity: "1", transform: "translateY(0)" } },
        pulse_dot: { "0%,100%": { opacity: "1" }, "50%": { opacity: "0.45" } },
        shimmer: { "100%": { transform: "translateX(100%)" } },
      },
      animation: {
        "fade-in": "fade-in 120ms ease-out",
        "slide-up": "slide-up 140ms cubic-bezier(0.2,0.8,0.2,1)",
        "pulse-dot": "pulse_dot 1.8s ease-in-out infinite",
        shimmer: "shimmer 1.4s infinite",
      },
    },
  },
  plugins: [require("tailwindcss-animate")],
} satisfies Config;
