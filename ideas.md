- Stored commands?
  - I.e. commands which can simply be selected, without any input from the tui
  - e.g. "Create draft PR" - creates a PR on github, with a good description, monitors that all tests pass, fixes in new commit if not.
  - e.g. "Address review" - look at all existing review comments, determine which should be addressed, fix each point in a separate commit, and then push all. End with printing a summary of comments which I can print as a response to each comment, including the commit hash of the commit which addresses
  


- I should create a workflow for working on agman with agman. e.g. merge to main and then delete the task?


  ## Problems

  - agman does not seem to work to edit agman...
  - we should only show the actions which are relevant for a task. e.g. start should not show for a task which is running

