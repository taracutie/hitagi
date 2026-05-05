import { UserCard } from "../components/UserCard";
import type { UserProfile } from "../types/user";

export async function loader({ params }: { params: { userId: string } }) {
  return fetch(`/api/users/${params.userId}`).then((response) => response.json());
}

export default function UserRoute({ user }: { user: UserProfile }) {
  return <UserCard user={user} />;
}
