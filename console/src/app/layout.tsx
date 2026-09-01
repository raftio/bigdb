import type { Metadata } from "next";
import { Archivo, JetBrains_Mono } from "next/font/google";
import { Providers } from "./providers";
import { themeScript } from "@/components/shell/theme";
import "./globals.css";

/* One grotesque UI face, one mono. Both self-hosted, both variable. */
const sans = Archivo({ subsets: ["latin"], weight: ["400", "500", "600"], display: "swap", variable: "--font-archivo" });
const mono = JetBrains_Mono({ subsets: ["latin"], display: "swap", variable: "--font-jetbrains" });

export const metadata: Metadata = {
  title: "bigdb Cloud",
  description: "The hosted control plane for big — a bitmap-native analytical database.",
};

export default function RootLayout({ children }: { children: React.ReactNode }) {
  return (
    <html lang="en" className={`${sans.variable} ${mono.variable}`} suppressHydrationWarning>
      <head><script dangerouslySetInnerHTML={{ __html: themeScript }} /></head>
      <body className="min-h-screen antialiased">
        <a href="#main" className="sr-only focus:not-sr-only focus:absolute focus:left-3 focus:top-3 focus:z-[100] focus:rounded focus:bg-accent focus:px-3 focus:py-1.5 focus:text-accent-fg">
          Skip to content
        </a>
        <Providers>{children}</Providers>
      </body>
    </html>
  );
}
