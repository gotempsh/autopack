package main

import (
	"fmt"
	"log"
	"net/http"
	"os"
)

func main() {
	port := os.Getenv("PORT")
	if port == "" {
		port = "3000"
	}

	http.HandleFunc("/", func(w http.ResponseWriter, _ *http.Request) {
		fmt.Fprintln(w, "hello from autopack")
	})

	log.Printf("listening on %s", port)
	log.Fatal(http.ListenAndServe(":"+port, nil))
}



