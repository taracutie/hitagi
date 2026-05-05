import type { UserProfile } from "../types/user";
import { useUserPreferences } from "../hooks/useUserPreferences";

export function UserCard({ user }: { user: UserProfile }) {
  const preferences = useUserPreferences(user.id);

  return (
    <article data-theme={preferences.theme}>
      <h2>{user.displayName}</h2>
      <p>{user.email}</p>
    </article>
  );
}
