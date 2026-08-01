(defproject clojure-app-example "1.0.0"
  :description "autopack Clojure example"
  :dependencies [[org.clojure/clojure "1.12.0"]]
  :main ^:skip-aot demo.core
  :profiles {:uberjar {:aot :all}})
