import { redirect } from "next/navigation";

/** The marketing site is a separate deployment; the console starts at its list. */
export default function Home() {
  redirect("/deployments");
}
