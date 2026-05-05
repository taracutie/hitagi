import { render, screen } from "@testing-library/react";
import { UserCard } from "./UserCard";

test("renders the user display name", () => {
  render(
    <UserCard user={{ id: "u1", displayName: "Ada Lovelace", email: "ada@example.com" }} />,
  );

  expect(screen.getByText("Ada Lovelace")).toBeTruthy();
});
