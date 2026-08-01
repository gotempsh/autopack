ThisBuild / scalaVersion := "3.6.2"

lazy val root = (project in file("."))
  .settings(
    name := "scala-app-example",
    assembly / assemblyJarName := "scala-app-example-assembly.jar"
  )
