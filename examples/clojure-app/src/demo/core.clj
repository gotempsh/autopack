(ns demo.core
  "Uses only com.sun.net.httpserver from the JDK, so the example exercises the
   uberjar build rather than a web framework's dependency graph."
  (:import (com.sun.net.httpserver HttpServer HttpHandler)
           (java.net InetSocketAddress))
  (:gen-class))

(def body "hello from autopack\n")

(defn -main [& _args]
  (let [port (Integer/parseInt (or (System/getenv "PORT") "3000"))
        server (HttpServer/create (InetSocketAddress. "0.0.0.0" port) 0)]
    (.createContext server "/"
      (reify HttpHandler
        (handle [_ exchange]
          (let [bytes (.getBytes body "UTF-8")]
            (.add (.getResponseHeaders exchange) "Content-Type" "text/plain")
            (.sendResponseHeaders exchange 200 (count bytes))
            (with-open [out (.getResponseBody exchange)]
              (.write out bytes))))))
    (.start server)
    (println "listening on" port)))
