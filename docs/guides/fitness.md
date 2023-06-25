# Fitness

Some pointer on Ryot and fitness tracking.

## Exercises

Before you get exercises tracking working, you need to import all exercises data.
Follow these steps to do so:

1. Make sure you have file storage integration working. This can be done by setting
the relevant `file_storage.*` configuration parameters.

2. Open your instance's `/graphql` endpoint. For example `https://ryot.fly.dev/graphql`.

3. Enter the following mutation in the editor and run it.

  ```graphql
  mutation DeployUpdateExerciseLibraryJob {
    deployUpdateExerciseLibraryJob
  }
  ```

**NOTE**: This needs to be run only once.